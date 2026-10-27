use std::fmt;

use serde::{Deserialize, Serialize};

/// Where a lease is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    /// The provider did not answer the start. The lease may or may not exist there;
    /// the table observes it until the provider says.
    Pending,
    /// The provider confirmed the lease and its host.
    Held,
    /// The lease is being released or has expired, and the provider has not yet
    /// confirmed the stop. Its booking stays counted until it does.
    Releasing,
}

impl fmt::Display for LeaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Held => "held",
            Self::Releasing => "releasing",
        })
    }
}
