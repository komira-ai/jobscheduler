//! The SQLite backend: the state store for `all-in-one` and for a single frontend.
//!
//! - **One writer.** A thread owns the only connection that writes. Calls queue to it. A
//!   call with no other queued behind it when the writer takes it is a group of its own,
//!   committed at once; when another is queued, the writer gathers the requests that
//!   arrive inside the commit window ([`Options::window`], 5 ms by default,
//!   [`crate::GROUP_COMMIT_WINDOW`]) into the group. A group is one `BEGIN IMMEDIATE`
//!   transaction, committed with one sync (WAL mode, `synchronous=FULL`), and only then
//!   does the writer answer every request in it. Compare-and-set, transactions and counters run
//!   inside that thread, so they are linearizable by construction.
//! - **Reads** use a connection of their own: in WAL mode they never wait for the writer
//!   and see only committed groups, so the change feed needs no horizon beyond the head.
//! - **One process.** The store holds an exclusive `flock` on `<file>.lock` for as long as
//!   it is open, and refuses a file on a network filesystem. A second store on the same
//!   file fails to open with [`SqliteError::Locked`]. The network-filesystem check reads
//!   the `statfs` type of the file's directory, so it is made on Linux only: on any other
//!   system nothing is refused, and keeping the file on a local disk is the operator's job.
//! - **Epoch.** Every open bumps the epoch recorded in the file ([`SqliteStore::epoch`]);
//!   [`restore`] does too, so a restored or swapped file never reuses an epoch the replaced
//!   file issued. The farm's epoch, the one `Welcome` names, is not this one: a frontend
//!   keeps the `meta/epoch` row, and after a restore the operator raises it with
//!   `POST /v1/epoch/bump`.
//! - **Backup and restore.** [`backup`] copies the file while a store writes it, into a
//!   single file that [`check_backup`] checks; [`restore`] (or [`restore_locked`], under
//!   a [`StoreLock`] the caller took) puts one back while no store has the file open.
//! - **Clock.** Expiry times are stored as nanoseconds on the store's clock, so the clock
//!   must mean the same thing after a restart: use [`crate::SystemClock`] in production.
//!
//! Commit latency, from the call queuing a write to its group being on disk, is kept in
//! [`CommitMetrics`] for the operator's metrics to export.

mod backup;
mod error;
mod guard;
mod read;
mod restore;
mod schema;
mod writer;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::Connection;

pub use backup::{Summary, backup, check_backup};
pub use error::SqliteError;
pub use guard::StoreLock;
pub(crate) use guard::recorded_holder;
pub use restore::{restore, restore_locked};
pub use schema::SCHEMA_VERSION;

use crate::GROUP_COMMIT_WINDOW;
use crate::clock::{Clock, StoreTime};
use crate::store::Store;
use crate::types::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, StoreError, Txn,
    Versioned,
};
use crate::writer::{Held, WriterLock};
use writer::{Reply, Request, Shared, Writer};

/// How the store groups writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// How long the writer waits, after the first request of a group, for more to join it.
    /// It waits only when another request is already queued as it takes the first: a lone
    /// request is committed at once.
    pub window: Duration,
    /// The group is committed as soon as it holds this many transactions, window or not.
    pub max_group: usize,
    /// A group whose commit latency passes this is counted in
    /// [`CommitMetrics::over_limit`].
    pub latency_limit: Duration,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            window: GROUP_COMMIT_WINDOW,
            max_group: 4096,
            latency_limit: Duration::from_millis(100),
        }
    }
}

/// Commit latency figures, for the operator's metrics. Latency runs from a call queuing
/// its write to the group it is in being on disk, so it includes the window, when the
/// group waited it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitMetrics {
    /// Groups committed.
    pub groups: u64,
    /// The most transactions any group held.
    pub largest_group: u64,
    /// The sum of the groups' latencies.
    pub total_latency: Duration,
    /// The longest any group took.
    pub max_latency: Duration,
    /// How many groups passed [`Options::latency_limit`].
    pub over_limit: u64,
}

/// The SQLite state store.
pub struct SqliteStore {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    epoch: u64,
    /// Replaced at drop, so the writer sees its queue close.
    requests: SyncSender<Request>,
    writer: Mutex<Option<JoinHandle<()>>>,
    reader: Mutex<Connection>,
    shared: Arc<Shared>,
    /// Held until the store is dropped; released after the writer is gone.
    lock: Mutex<Option<guard::LockFile>>,
}

impl SqliteStore {
    /// Opens the state file `path`, creating it when it does not exist, with the system's
    /// own filesystem check. Bumps the epoch.
    ///
    /// # Errors
    /// [`SqliteError`]: the file is on a network filesystem, another process holds it,
    /// it is not a state store this binary reads, or a call failed.
    pub fn open(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<SqliteStore, SqliteError> {
        SqliteStore::open_checked(path, clock, options, &guard::filesystem_type)
    }

    /// [`SqliteStore::open`] under a writer lock already taken on this file
    /// ([`WriterLock::acquire`] with [`crate::WriterTarget::Sqlite`]). The store adopts the
    /// held `flock` instead of taking a second one (which [`SqliteStore::open`] does, and
    /// which is refused with [`SqliteError::Locked`] while the writer lock is held, in this
    /// process as in any other), and keeps it until it is dropped: the file is locked from
    /// before the open to the end of the store, with no gap a second process could take it
    /// in. The lock is matched to the file by the file's one name, whichever spelling of
    /// it the lock and the open were given. Bumps the epoch.
    ///
    /// # Errors
    /// [`SqliteError::NotThisFilesLock`] when `lock` was taken on another state file or on a
    /// Postgres store; otherwise as [`SqliteStore::open`]. The lock is released on any error.
    pub fn open_locked(
        path: &Path,
        lock: WriterLock,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<SqliteStore, SqliteError> {
        SqliteStore::opened(path, clock, options, &guard::filesystem_type, Some(lock))
    }

    /// [`SqliteStore::open`] with the filesystem check's probe (the type number of a
    /// directory's filesystem) given, so a test can present a network filesystem.
    fn open_checked(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
        probe: &dyn Fn(&Path) -> io::Result<u32>,
    ) -> Result<SqliteStore, SqliteError> {
        SqliteStore::opened(path, clock, options, probe, None)
    }

    /// Opens the store, taking the file's lock, or adopting `writer` when one is given.
    fn opened(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
        probe: &dyn Fn(&Path) -> io::Result<u32>,
        writer: Option<WriterLock>,
    ) -> Result<SqliteStore, SqliteError> {
        match SqliteStore::start(path, clock, options, probe, writer) {
            Ok(store) => Ok(store),
            Err(error) => Err(error.at(path)),
        }
    }

    fn start(
        path: &Path,
        clock: Arc<dyn Clock>,
        options: Options,
        probe: &dyn Fn(&Path) -> io::Result<u32>,
        writer: Option<WriterLock>,
    ) -> Result<SqliteStore, SqliteError> {
        guard::refuse_network_filesystem(path, probe)?;
        guard::refuse_hard_link(path)?;
        let lock = match writer {
            Some(writer) => adopt(path, writer)?,
            None => guard::take_lock(path)?.0,
        };
        let conn = Connection::open(path)?;
        schema::init(&conn)?;
        schema::configure(&conn)?;
        let epoch = schema::bump_epoch(&conn, 0)?;
        let head = schema::head(&conn)?;
        let shared = Arc::new(Shared::default());
        let reader = Connection::open(path)?;
        schema::configure(&reader)?;
        reader.pragma_update(None, "query_only", true)?;
        reader.set_prepared_statement_cache_capacity(read::STATEMENTS);

        let (requests, queue) = mpsc::sync_channel(options.max_group.max(1));
        let writer = Writer {
            conn,
            clock: Arc::clone(&clock),
            options,
            shared: Arc::clone(&shared),
            head: writer::uint(head),
        };
        let handle = writer.spawn(queue)?;
        Ok(SqliteStore {
            path: path.to_owned(),
            clock,
            epoch,
            requests,
            writer: Mutex::new(Some(handle)),
            reader: Mutex::new(reader),
            shared,
            lock: Mutex::new(Some(lock)),
        })
    }

    /// The state file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The clock the store was opened with.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// The store's incarnation: one past the epoch of the previous open (or restore) of
    /// the file. A daemon that sees a new epoch kills the leases of the old one.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The commit latency figures so far.
    #[must_use]
    pub fn commit_metrics(&self) -> CommitMetrics {
        *self
            .shared
            .metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a request for the writer and waits for its group to be on disk.
    fn call(&self, txns: Vec<Txn>, reap: bool, trim: Option<u64>) -> Result<Reply, StoreError> {
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request {
            txns,
            reap,
            trim,
            queued: Instant::now(),
            reply,
        };
        self.shared.queue(&self.requests, request);
        answer.recv().ok().ok_or_else(gone)?
    }

    fn reader(&self) -> std::sync::MutexGuard<'_, Connection> {
        held(&self.reader)
    }
}

/// The `flock` of `writer`, when it is the writer lock of the state file `path`.
fn adopt(path: &Path, writer: WriterLock) -> Result<guard::LockFile, SqliteError> {
    let own = guard::canonical_lock(path)?;
    match writer.held {
        Held::Sqlite(lock) if lock.lock_file() == own => Ok(lock.into_file()),
        _ => Err(SqliteError::NotThisFilesLock {
            path: path.to_owned(),
            lock: own,
        }),
    }
}

fn held<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answer to a call the writer cannot take or will never answer.
fn gone() -> StoreError {
    StoreError::Failed("the state store's writer has stopped".to_owned())
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl Drop for SqliteStore {
    /// Stops the writer after the calls already queued, then releases the file lock.
    fn drop(&mut self) {
        // Closing the queue ends the writer after the requests already in it.
        drop(std::mem::replace(
            &mut self.requests,
            mpsc::sync_channel(0).0,
        ));
        // A writer that panicked has nothing more to say to us.
        let _ = held(&self.writer).take().map(JoinHandle::join);
        drop(held(&self.lock).take());
    }
}

impl Store for SqliteStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Ok(self.call(batch, false, None)?.commits)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        read::get(&self.reader(), key, self.clock.now())
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        read::scan_prefix(&self.reader(), prefix, self.clock.now())
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        read::scan_range(&self.reader(), scan, self.clock.now())
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        read::changes(&mut self.reader(), after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        Ok(self.call(Vec::new(), false, Some(before.0))?.trimmed)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Ok(self.call(Vec::new(), true, None)?.reaped)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats {
            txns: self.shared.txns.load(Ordering::Acquire),
            syncs: self.shared.commits.load(Ordering::Acquire),
        }
    }

    fn now(&self) -> StoreTime {
        self.clock.now()
    }
}

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod group_tests;
#[cfg(test)]
mod tests;
