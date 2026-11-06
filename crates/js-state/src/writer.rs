//! One active writer per store (design sections 9 and 10.2): the lock the writer holds for
//! its life, one type over both backends.
//!
//! - **Postgres** (feature `postgres`): a session-level advisory lock on the key
//!   `postgres::WRITER_LOCK_KEY`,
//!   held on a connection of its own (`postgres/writer.rs`). Once it holds the lock, the
//!   acquire increments the store's writer generation and keeps the new value
//!   ([`WriterLock::generation`]): the holder opens its store with it
//!   (`postgres::PostgresStore::fenced`), and every commit of that store checks the row
//!   still holds it.
//! - **SQLite**: the `flock` on `<file>.lock` a store holds while it is open
//!   ([`StoreLock`]). [`crate::sqlite::SqliteStore::open_locked`] opens the store under a
//!   lock taken here, so the lock is held from before the open until the store is dropped.
//!
//! [`WriterLock::acquire`] tries once and never waits: whether to refuse to start or to try
//! again later is the caller's decision.
//!
//! [`WriterLock::probe`] says whether the lock is held, and by whom, without taking it,
//! even for an instant (a lock taken to look at it could make a writer starting in that
//! instant refuse): on Postgres from `pg_locks`, on SQLite from the token every holder
//! writes into `<file>.lock` once it has the `flock` (`<pid>@<micros>`, the process and
//! when it took the lock) and empties before it lets the lock go.

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(feature = "postgres")]
use crate::postgres::PostgresConfig;
#[cfg(feature = "postgres")]
use crate::postgres::writer::LockSession;
use crate::sqlite::{SqliteError, StoreLock, recorded_holder};
use crate::types::StoreError;

/// The store a [`WriterLock`] is taken for.
#[derive(Clone, Debug)]
pub enum WriterTarget {
    /// The Postgres database the configuration names (the lock covers the database, not
    /// only the configured schema). Boxed: the configuration is many times the size of a
    /// path.
    #[cfg(feature = "postgres")]
    Postgres(Box<PostgresConfig>),
    /// The SQLite state file at this path (any spelling of it: the lock is beside the
    /// file's one name).
    Sqlite(PathBuf),
}

/// Why [`WriterLock::acquire`] did not take the lock. No variant carries a connection URL
/// or a password.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LockRefused {
    /// Another session or process holds the lock: another writer is running on the store.
    #[error("another process holds the store's writer lock")]
    Held,
    /// The lock could not be tried: the server cannot be reached or refused the login, or a
    /// file call failed. The text says which.
    #[error("the store's writer lock could not be taken: {0}")]
    Failed(String),
}

/// Why [`WriterLock::probe`] could not say whether the lock is held. The text never
/// carries a connection URL or a password.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the store's writer lock could not be probed: {0}")]
pub struct ProbeFailed(String);

/// Whether a store's writer lock is held, as [`WriterLock::probe`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockState {
    /// No holder was found.
    Free,
    /// A holder has the lock.
    Held {
        /// Who holds it.
        holder: LockHolder,
    },
}

/// Who holds a writer lock, as an opaque text for logs and for comparing two probes: on
/// Postgres `<pid>@<backend start>` (the holder's backend, and when it started in
/// microseconds since the Unix epoch), or the pid alone when the probing role may not see
/// the holder's session; on SQLite the holder's token, `<pid>@<micros>` (the holding
/// process, and when it took the lock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockHolder(String);

impl LockHolder {
    /// The holder's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LockHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The generation a Postgres writer lock took: the value of the store's writer generation row
/// after this holder incremented it. Each acquire takes a greater one than every acquire
/// before it on the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriterGeneration(i64);

impl WriterGeneration {
    /// The generation `value` (the row's value).
    #[must_use]
    pub const fn new(value: i64) -> WriterGeneration {
        WriterGeneration(value)
    }

    /// The row's value.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// The lock of the one active writer of a store, held until this value is dropped.
pub struct WriterLock {
    pub(crate) held: Held,
}

/// What holds a [`WriterLock`].
pub(crate) enum Held {
    #[cfg(feature = "postgres")]
    Postgres(Box<LockSession>),
    Sqlite(StoreLock),
}

impl WriterLock {
    /// Takes the writer lock of `target`, once, without waiting.
    ///
    /// # Errors
    /// [`LockRefused::Held`] when another holder has it; [`LockRefused::Failed`] when it
    /// could not be tried.
    pub fn acquire(target: &WriterTarget) -> Result<WriterLock, LockRefused> {
        let held = match target {
            #[cfg(feature = "postgres")]
            WriterTarget::Postgres(config) => Held::Postgres(Box::new(LockSession::take(config)?)),
            WriterTarget::Sqlite(path) => {
                Held::Sqlite(StoreLock::take(path).map_err(|error| refused(path, error))?)
            }
        };
        Ok(WriterLock { held })
    }

    /// Whether the writer lock of `target` is held, and by whom, found without taking the
    /// lock or changing it. The answer can be stale the moment it is given.
    ///
    /// On Postgres it reads `pg_locks` on a connection of its own. On SQLite it reads the
    /// token in `<file>.lock`: no lock file, an empty one, or a token whose process does
    /// not exist (a holder that ended without emptying it) is [`LockState::Free`]. A SQLite
    /// holder that has taken the `flock` but not yet written its token, one whose drop has
    /// emptied the token but not yet unlocked, or one of a release that wrote none, also
    /// reads as free. Process ids are those of the probing process's pid namespace, so a
    /// SQLite probe runs in the holder's; a token left by a holder that ended without its
    /// drop, whose process id another process has since taken, reads as held until the
    /// next holder overwrites it.
    ///
    /// # Errors
    /// [`ProbeFailed`] when the server cannot be reached or refuses the login or the query,
    /// or the lock file cannot be read.
    pub fn probe(target: &WriterTarget) -> Result<LockState, ProbeFailed> {
        let holder = match target {
            #[cfg(feature = "postgres")]
            WriterTarget::Postgres(config) => crate::postgres::writer::probe(config),
            WriterTarget::Sqlite(path) => recorded_holder(path).map_err(|error| error.to_string()),
        }
        .map_err(ProbeFailed)?;
        Ok(match holder {
            Some(holder) => LockState::Held {
                holder: LockHolder(holder),
            },
            None => LockState::Free,
        })
    }

    /// The writer generation this holder took, which its store is opened with
    /// (`postgres::PostgresStore::fenced`). `None` on SQLite, where the file lock alone keeps
    /// a second writer out (the lock lasts as long as the process that holds it).
    #[must_use]
    pub fn generation(&self) -> Option<WriterGeneration> {
        match &self.held {
            #[cfg(feature = "postgres")]
            Held::Postgres(session) => Some(session.generation()),
            Held::Sqlite(_) => None,
        }
    }

    /// Whether the lock is still held, for a caller to ask on every tick. On Postgres it
    /// runs a trivial query on the lock's own session, which fails once that session is
    /// gone (and the server has released the lock with it). On SQLite it always succeeds:
    /// the `flock` lasts as long as this process keeps the lock file open, and no other
    /// process can take it while the lock file is there (removing the lock file by hand
    /// would let a new one be locked beside it; nothing in this crate removes it).
    ///
    /// # Errors
    /// [`StoreError`] when the Postgres session is gone or the query fails.
    pub fn check(&mut self) -> Result<(), StoreError> {
        match &mut self.held {
            #[cfg(feature = "postgres")]
            Held::Postgres(session) => session.check(),
            Held::Sqlite(_) => Ok(()),
        }
    }
}

impl fmt::Debug for WriterLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match &self.held {
            #[cfg(feature = "postgres")]
            Held::Postgres(_) => "postgres",
            Held::Sqlite(_) => "sqlite",
        };
        f.debug_struct("WriterLock")
            .field("backend", &backend)
            .finish_non_exhaustive()
    }
}

/// The lock of the state file `path` that could not be taken: held by another descriptor,
/// or a failed call (a file call's text is given the path it was about).
fn refused(path: &Path, error: SqliteError) -> LockRefused {
    match error {
        SqliteError::Locked { .. } => LockRefused::Held,
        SqliteError::Io(text) => LockRefused::Failed(format!("{}: {text}", path.display())),
        other => LockRefused::Failed(other.to_string()),
    }
}
