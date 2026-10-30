use std::io;
use std::path::PathBuf;

use js_provider::RefusalCode;
use js_types::LeaseId;

use crate::LeaseState;

/// The lease file could not be written.
///
/// The change that led to the save is already applied in memory, so the table is ahead of
/// the file: the file still holds what the last successful save wrote. The table does not
/// roll back. The next successful save (a tick retries it) writes the whole table and
/// brings the file level; a caller that wants what the file says drops the table and
/// opens it again. See the crate docs, "When a save fails".
#[derive(Debug, thiserror::Error)]
#[error("lease file {}: cannot write: {source}", path.display())]
pub struct SaveError {
    /// The file the table was writing.
    pub path: PathBuf,
    /// What the operating system said.
    pub source: io::Error,
}

/// The settings were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// The default ttl was zero.
    #[error("the default ttl must be positive")]
    DefaultNotPositive,
    /// The default ttl was above the maximum ttl.
    #[error("the default ttl must not exceed the maximum ttl")]
    DefaultAboveMax,
    /// The prepare timeout was zero.
    #[error("the prepare timeout must be positive")]
    PrepareNotPositive,
    /// The ceiling on the maximum wall time was zero.
    #[error("the ceiling on the maximum wall time must be positive")]
    MaxWallNotPositive,
}

/// The table could not be opened over its file.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The file exists and could not be read.
    #[error("lease file {}: cannot read: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        source: io::Error,
    },
    /// The file does not exist and the table was not asked to initialise it: either
    /// this is a first boot that has to say so, or the state volume was lost.
    #[error("lease file {}: does not exist (only a first boot may create it)", path.display())]
    Missing {
        /// The file.
        path: PathBuf,
    },
    /// The lock beside the file could not be taken. An error of kind `WouldBlock`
    /// means another table holds it: two tables over one file would issue the same ids.
    #[error("lease file lock {}: cannot lock: {source}", path.display())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// What the operating system said.
        source: io::Error,
    },
    /// The file was read but is not a lease file, or holds a lease that is not valid.
    /// The table never starts empty over a file it cannot read.
    #[error("lease file {}: corrupt: {reason}", path.display())]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
    /// The file's term is the largest the file can hold, so this boot cannot take the
    /// next one and no id of it could be told apart from an earlier boot's.
    #[error("lease file {}: the term cannot advance: the ids of this file are used up", path.display())]
    TermExhausted {
        /// The file.
        path: PathBuf,
    },
    /// The file could not be written after loading. No table is returned;
    /// the file is as the last successful save left it, and the open can be tried again.
    #[error(transparent)]
    Save(#[from] SaveError),
}

/// An acquire did not produce a held lease.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// The ttl was zero.
    #[error("the ttl must be positive")]
    TtlNotPositive,
    /// The request's maximum wall time is above the ceiling the table was set with
    /// ([`crate::Settings::with_max_wall`]). Nothing was recorded, no lease id was used and
    /// the provider was not called.
    #[error("the maximum wall time is above the ceiling of {} s", .ceiling.as_secs())]
    WallAboveCeiling {
        /// The ceiling.
        ceiling: std::time::Duration,
    },
    /// The sequence has reached the largest value the file can hold, so no further id
    /// can be issued. Nothing was recorded and the provider was not called.
    #[error("lease ids are used up: the next sequence cannot be stored")]
    IdsExhausted,
    /// The request or the provider was refused, with a plain constant code.
    #[error("the start was refused: {}", .0.as_str())]
    Refused(RefusalCode),
    /// The provider gave no answer to the start. The lease is recorded as pending
    /// and the table resolves it by observing it; it is not held.
    #[error("lease {0} is pending: the provider did not answer the start")]
    Unconfirmed(LeaseId),
    /// The lease file could not be written before the provider was called, so it was
    /// not called. The record stays pending in memory, retried by [`crate::LeaseTable::tick`]
    /// and ended by its expiry. A save that fails after the provider answered is not an
    /// error here: the client gets the answer and the tick report carries the failure.
    #[error(transparent)]
    Save(#[from] SaveError),
}

/// A renew did not extend a lease.
#[derive(Debug, thiserror::Error)]
pub enum RenewError {
    /// The ttl was zero.
    #[error("the ttl must be positive")]
    TtlNotPositive,
    /// The table has no such lease: never acquired, released or expired and freed.
    #[error("lease {0} is not known")]
    Unknown(LeaseId),
    /// The lease is not held: it is pending or being released.
    #[error("lease {id} is {state}, not held")]
    NotHeld {
        /// The lease.
        id: LeaseId,
        /// What it is instead.
        state: LeaseState,
    },
    /// The lease's expiry has passed and the next tick will release it.
    #[error("lease {0} has expired")]
    Expired(LeaseId),
    /// The lease file could not be written. The renewal took effect in memory and the file
    /// is behind it until the next successful save (see [`SaveError`]); reopening the
    /// table returns to the file's expiry.
    #[error(transparent)]
    Save(#[from] SaveError),
}

/// A release did not run.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The table has no such lease.
    #[error("lease {0} is not known")]
    Unknown(LeaseId),
    /// The lease file could not be written before the stop. The lease stays marked
    /// releasing in memory and the next tick stops it. The file is behind
    /// until a save succeeds (see [`SaveError`]); reopening returns to what it says.
    #[error(transparent)]
    Save(#[from] SaveError),
}
