//! The one writer of a SQLite store: the lock the process holds for its life. A SQLite file
//! is written by one process, so it takes the `flock` on `<file>.lock` while it is open
//! ([`StoreLock`]); [`crate::sqlite::SqliteStore::open_locked`] opens the store under a lock
//! taken here, so the lock is held from before the open until the store is dropped.
//!
//! A Postgres store has no writer lock: any number of processes write it at once, and each
//! commit is correct on its own. [`WriterTarget::Postgres`] exists so that a caller names
//! the store it means, and [`WriterLock::probe`] says who is serving it.
//!
//! [`WriterLock::acquire`] tries once and never waits: whether to refuse to start or to try
//! again later is the caller's decision.
//!
//! [`WriterLock::probe`] says whether a SQLite store's lock is held, and by whom, without
//! taking it, even for an instant (a lock taken to look at it could make a writer starting in
//! that instant refuse): from the token every holder writes into `<file>.lock` once it has
//! the `flock` (`<pid>@<micros>`, the process and when it took the lock) and empties before
//! it lets the lock go. On Postgres it says which process is connected
//! ([`crate::postgres::serving`]).

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(feature = "postgres")]
use crate::postgres::PostgresConfig;
use crate::sqlite::{SqliteError, StoreLock, recorded_holder};

/// The store whose writer [`WriterLock::probe`] looks for.
#[derive(Clone, Debug)]
pub enum WriterTarget {
    /// The Postgres database the configuration names. Boxed: the configuration is many
    /// times the size of a path.
    #[cfg(feature = "postgres")]
    Postgres(Box<PostgresConfig>),
    /// The SQLite state file at this path (any spelling of it: the lock is beside the
    /// file's one name).
    Sqlite(PathBuf),
}

/// Why [`WriterLock::acquire`] did not take the lock.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LockRefused {
    /// Another process holds the lock: another writer is running on the store.
    #[error("another process holds the store's writer lock")]
    Held,
    /// The lock could not be tried: a file call failed. The text says which.
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

/// The lock of the one writer of a SQLite store, held until this value is dropped.
pub struct WriterLock {
    pub(crate) held: Held,
}

/// What holds a [`WriterLock`].
pub(crate) enum Held {
    Sqlite(StoreLock),
}

impl WriterLock {
    /// Takes the writer lock of the SQLite state file at `path`, once, without waiting.
    ///
    /// # Errors
    /// [`LockRefused::Held`] when another holder has it; [`LockRefused::Failed`] when it
    /// could not be tried.
    pub fn acquire(path: &Path) -> Result<WriterLock, LockRefused> {
        let held = Held::Sqlite(StoreLock::take(path).map_err(|error| refused(path, error))?);
        Ok(WriterLock { held })
    }

    /// Whether the writer lock of `target` is held, and by whom, found without taking the
    /// lock or changing it. The answer can be stale the moment it is given.
    ///
    /// On Postgres, which has no lock, it says which process is connected under the
    /// configuration's application name ([`crate::postgres::serving::probe`]). On SQLite it reads the
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
            WriterTarget::Postgres(config) => crate::postgres::serving::probe(config),
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
}

impl fmt::Debug for WriterLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriterLock")
            .field("backend", &"sqlite")
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
