//! The store the server's state is kept in (`--store`, `server.store`).
//!
//! - **Postgres**: any number of servers open the same database and serve at once. The server
//!   takes no lock and no generation: every decision it makes is a transaction of the store
//!   whose preconditions hold for however many servers run it (the job store's versioned rows,
//!   its placement reservation, the periodic claims), so a second server that starts, a server
//!   that stops and one that is replaced beside it need no hand-over.
//! - **SQLite**: one process writes the file. The server takes the `flock` beside it first
//!   thing at start, before it reads anything from the store and before it binds a listener
//!   (the readiness listener of `--ready-listen` excepted, which is bound before the lock so
//!   that it can say the server waits for it). The lock is tried once: when another process
//!   holds it the start is refused with one line naming the lock, and nothing else is done.
//!   With `--ready-listen` the server instead tries again at each tick of its retry ticker for
//!   as long as the lock is held ([`ServerStore::open_waiting`]). The store holds the lock from
//!   before its open to its end ([`SqliteStore::open_locked`]).
//!
//! What the store keeps: the host bindings, with their taints and cordons, and the join
//! tokens' digests ([`crate::host_keys`]), and the scaler's records and the hosts the
//! registry refused to remove (`acquires/scaler/records`, `acquires/scaler/unremoved`), read
//! once at start, and, with the job service on, the job store (jobs, allocations, marks,
//! taints and the change feed) and, with usage on, the usage rows (node minutes, profiles,
//! the history of ended allocations). Without the setting the job store is the SQLite file
//! `<state file>.jobs` and the usage the file `usage.db`; with it neither file is made, opened
//! or read, and a pair left by an earlier start stays where it is, unread.
//!
//! The first start over a store imports the four files a server without the setting keeps
//! beside the state file (`.agents`, `.joins`, `.scaler`, `.scaler.unremoved`) once, before
//! anything is read from the store ([`ServerStore::import`]).

use std::fs::{OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use js_state::postgres::{Listener, PostgresConfig, PostgresStore, reach};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, LockRefused, Store, SystemClock, WriterLock, WriterTarget};
use tracing::info;

use crate::clock::SharedClock;
use crate::config::{StoreClock, StoreNames, StoreSetting};
use crate::error::StartupError;
use crate::ticker::{Tick, Ticker};
use crate::util::blocking;

mod import;

/// The store the server keeps its state in, open.
pub(crate) struct ServerStore {
    /// The store, for the parts of the server that keep their state in it.
    store: Arc<dyn Store>,
    /// `--store` or `server.store`, for the refusals.
    names: StoreNames,
    /// Whether the store holds a file lock (SQLite) for as long as it is open.
    locked: bool,
    /// The database of a store other servers share (Postgres), which this server listens to for
    /// their notices ([`ServerStore::listener`]).
    shared: Option<PostgresConfig>,
    /// The same store when it is Postgres, for what it counts of its own work
    /// ([`ServerStore::tally`]).
    postgres: Option<Arc<PostgresStore>>,
}

/// What a Postgres store counted of its own work since it was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoreTally {
    /// The messages carrying statements it sent, each a round trip
    /// ([`PostgresStore::statements`]).
    pub(crate) statements: u64,
    /// The threads it started to make a call away from the runtime
    /// ([`PostgresStore::threads_started`]).
    pub(crate) threads: u64,
}

impl ServerStore {
    /// Opens the store of `setting`. A SQLite file is locked first (a lock another process
    /// holds refuses the start) and made owner-only, as the server's other files are.
    ///
    /// # Errors
    ///
    /// A refusal naming the setting: another process holds the SQLite file's lock, the lock
    /// could not be tried, or the store could not be opened. None repeats the connection URL.
    pub(crate) fn open(setting: StoreSetting) -> Result<Self, StartupError> {
        let StoreSetting {
            clock,
            target,
            names,
            notices,
        } = setting;
        match target {
            WriterTarget::Sqlite(file) => {
                let lock = WriterLock::acquire(&file).map_err(|refused| names.refuse(refused))?;
                Self::sqlite(&file, lock, clock, names)
            }
            WriterTarget::Postgres(config) => Self::postgres(&config, (clock, notices), names),
        }
    }

    /// [`ServerStore::open`] for a server that waits: while another process holds the SQLite
    /// file's lock, logs once that the server waits and tries again at each tick of `retry`,
    /// finishing the tick when the attempt it asked for is done. The tries and the open run
    /// off the runtime's threads. A Postgres store has no lock to wait for.
    ///
    /// # Errors
    ///
    /// As [`ServerStore::open`], except that a lock another process holds is waited for, not
    /// refused.
    pub(crate) async fn open_waiting<R: Ticker>(
        setting: StoreSetting,
        mut retry: R,
    ) -> Result<Self, StartupError> {
        let StoreSetting {
            clock,
            target,
            names,
            notices,
        } = setting;
        match target {
            WriterTarget::Sqlite(file) => {
                let lock = wait_for_lock(&file, names, &mut retry).await?;
                blocking(move || Self::sqlite(&file, lock, clock, names)).await
            }
            WriterTarget::Postgres(config) => {
                blocking(move || Self::postgres(&config, (clock, notices), names)).await
            }
        }
    }

    /// The SQLite file `file`, opened under its lock.
    fn sqlite(
        file: &Path,
        lock: WriterLock,
        clock: StoreClock,
        names: StoreNames,
    ) -> Result<Self, StartupError> {
        let store = open_sqlite(file, lock, clock.0).map_err(|err| opened(names, err))?;
        info!("the store is open and its file lock is held");
        Ok(Self {
            store: Arc::new(store),
            names,
            locked: true,
            shared: None,
            postgres: None,
        })
    }

    /// The Postgres database `config` names, listened to for the other servers' notices when
    /// `notices`.
    fn postgres(
        config: &PostgresConfig,
        (clock, notices): (StoreClock, bool),
        names: StoreNames,
    ) -> Result<Self, StartupError> {
        // The login first: a refusal of it is told by what to do about it, since the server's
        // own words quote the role and the database.
        reach(config).map_err(|err| opened(names, err.to_string()))?;
        let store = PostgresStore::open(config).map_err(|err| opened(names, err.to_string()))?;
        let store = match clock.0 {
            Some(clock) => store.with_clock(clock),
            None => store,
        };
        info!("the store is open");
        let store = Arc::new(store);
        Ok(Self {
            store: Arc::clone(&store) as Arc<dyn Store>,
            names,
            locked: false,
            shared: notices.then(|| config.clone()),
            postgres: Some(store),
        })
    }

    /// Whether the store holds a file lock for as long as it is open (SQLite).
    pub(crate) fn is_locked(&self) -> bool {
        self.locked
    }

    /// The first start's import of the files beside the state file at `state_file`, once,
    /// and a no-op at every later start (see the `import` module), the cordon ends the file
    /// holds on `clock` (the server's) written on the store's. `records_needed` is true
    /// for a live elastic pool that is not at its first start, which does not start without
    /// its records.
    ///
    /// # Errors
    ///
    /// A file's refusal, naming the file, or the store could not be read or written, naming
    /// the store's setting: nothing was imported.
    pub(crate) fn import(
        &self,
        (state_file, clock): (&Path, &SharedClock),
        records_needed: bool,
    ) -> Result<(), StartupError> {
        let refuse = |reason: String| self.refuse(reason);
        import::run(&*self.store, (state_file, clock), records_needed, &refuse)
    }

    /// A refusal of the start for `reason`, naming the store's flag or key: what the store
    /// holds cannot be read as written.
    pub(crate) fn refuse(&self, reason: impl std::fmt::Display) -> StartupError {
        self.names.refuse(reason)
    }

    /// The store, shared.
    pub(crate) fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }

    /// What the store counted of its own work since it was opened; `None` for SQLite, which
    /// counts neither.
    pub(crate) fn tally(&self) -> Option<StoreTally> {
        self.postgres.as_ref().map(|store| StoreTally {
            statements: store.statements(),
            threads: store.threads_started(),
        })
    }

    /// A listener of the notices the servers of a shared store (Postgres) send on the job
    /// store's channels ([`js_jobs::CHANNELS`]); `None` for a store this process alone writes,
    /// which no other server tells anything.
    pub(crate) fn listener(&self) -> Option<Listener> {
        let config = self.shared.as_ref()?;
        Some(Listener::new(config, &js_jobs::CHANNELS))
    }
}

/// Takes the lock of the SQLite file `file`, logging once that it waits and trying again at
/// each tick of `retry` while another process holds it, finishing the tick when the attempt
/// it asked for is done.
async fn wait_for_lock<R: Ticker>(
    file: &Path,
    names: StoreNames,
    retry: &mut R,
) -> Result<WriterLock, StartupError> {
    let mut asked: Option<Tick> = None;
    let mut waiting = false;
    loop {
        let target = file.to_path_buf();
        let tried = blocking(move || WriterLock::acquire(&target)).await;
        asked.take().into_iter().for_each(Tick::finish);
        match tried {
            Ok(lock) => return Ok(lock),
            Err(LockRefused::Held) => {
                if !waiting {
                    info!("waiting for the store's writer lock");
                    waiting = true;
                }
                asked = Some(retry.next().await);
            }
            Err(refused) => return Err(names.refuse(refused)),
        }
    }
}

/// Opens the SQLite store at `file` under `lock`, made owner-only first.
fn open_sqlite(
    file: &Path,
    lock: WriterLock,
    clock: Option<Arc<dyn Clock>>,
) -> Result<SqliteStore, String> {
    let made = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(file)
        .and_then(|opened| opened.set_permissions(Permissions::from_mode(0o600)));
    made.map_err(|err| format!("{}: {err}", file.display()))?;
    let clock = clock.unwrap_or_else(|| Arc::new(SystemClock::default()));
    SqliteStore::open_locked(file, lock, clock, Options::default()).map_err(|err| err.to_string())
}

/// The refusal of the start for a store that could not be opened: `err` says why.
fn opened(names: StoreNames, err: String) -> StartupError {
    names.refuse(format!("the store could not be opened: {err}"))
}
