//! The clock the scaler is given.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// An instant: milliseconds since the Unix epoch, as the caller's clock reads them. The
/// scaler reads no clock; the caller passes one in, so a records file means the same moment
/// after a restart and a test moves time by building a later value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Millis(u64);

impl Millis {
    /// The instant `millis` after the epoch.
    #[must_use]
    pub fn new(millis: u64) -> Self {
        Self(millis)
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub fn as_millis(self) -> u64 {
        self.0
    }

    /// This instant plus `after`, saturating at the largest instant.
    #[must_use]
    pub fn plus(self, after: Duration) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(after.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// How long after `earlier` this instant is; zero if it is not after it.
    #[must_use]
    pub fn since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}
