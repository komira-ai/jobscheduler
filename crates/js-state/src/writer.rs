//! One active writer per store (design sections 9 and 10.2): the lock the writer holds for
//! its life, one type over both backends.
//!
//! - **Postgres** (feature `postgres`): a session-level advisory lock on the key
//!   `postgres::WRITER_LOCK_KEY`,
//!   held on a connection of its own (`postgres/writer.rs`).
//! - **SQLite**: the `flock` on `<file>.lock` a store holds while it is open
//!   ([`StoreLock`]). [`crate::sqlite::SqliteStore::open_locked`] opens the store under a
//!   lock taken here, so the lock is held from before the open until the store is dropped.
//!
//! [`WriterLock::acquire`] tries once and never waits: whether to refuse to start or to try
//! again later is the caller's decision.

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(feature = "postgres")]
use crate::postgres::PostgresConfig;
#[cfg(feature = "postgres")]
use crate::postgres::writer::LockSession;
use crate::sqlite::{SqliteError, StoreLock};
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
