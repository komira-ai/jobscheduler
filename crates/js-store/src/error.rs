//! Why the state store could not be read or written.

use js_state::{Conflict, StoreError};

/// Why the state store could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The backend failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row changed under the writer.
    #[error("a row changed under the scheduler: {0}")]
    Conflict(#[from] Conflict),
    /// The row a write depends on is not there: the write is stale.
    #[error("the state store has no {0}: the write is stale")]
    Missing(String),
    /// A row cannot be read.
    #[error("the state store row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
}

impl StateError {
    /// The error for the row at `key`, which is not what its kind of row holds.
    pub(crate) fn corrupt(key: &str, why: &str) -> Self {
        Self::Corrupt {
            key: key.to_owned(),
            why: why.to_owned(),
        }
    }
}
