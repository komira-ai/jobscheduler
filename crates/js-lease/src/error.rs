use std::io;
use std::path::PathBuf;

use js_provider::RefusalCode;
use js_types::LeaseId;

use crate::LeaseState;

/// The lease file could not be written.
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
    /// The file was read but is not a lease file, or holds a lease that is not valid.
    /// The table never starts empty over a file it cannot read.
    #[error("lease file {}: corrupt: {reason}", path.display())]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
    /// The file could not be written after loading.
    #[error(transparent)]
    Save(#[from] SaveError),
}

/// An acquire did not produce a held lease.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// The ttl was zero.
    #[error("the ttl must be positive")]
    TtlNotPositive,
    /// The request or the provider was refused, with a plain constant code.
    #[error("the start was refused: {}", .0.as_str())]
    Refused(RefusalCode),
    /// The provider gave no answer to the start. The lease is recorded as pending
    /// and the table resolves it by observing it; it is not held.
    #[error("lease {0} is pending: the provider did not answer the start")]
    Unconfirmed(LeaseId),
    /// The lease file could not be written. The table keeps its state in memory, and
    /// a lease the provider started ends by expiry if the client never learns of it.
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
    /// The lease file could not be written. The renewal took effect in memory.
    #[error(transparent)]
    Save(#[from] SaveError),
}

/// A release did not run.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The table has no such lease.
    #[error("lease {0} is not known")]
    Unknown(LeaseId),
    /// The lease file could not be written. The lease stays marked releasing in memory
    /// and the next tick stops it.
    #[error(transparent)]
    Save(#[from] SaveError),
}
