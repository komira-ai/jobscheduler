//! The clock the scaler is given.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The largest count of milliseconds an instant holds: the largest a signed 64-bit
/// integer holds, which is what a lease clock's timestamp (`js_lease::Timestamp`) and a
/// TOML file hold. A larger count saturates to it, so an instant converts to the lease
/// clock's without overflow.
const MAX_MILLIS: u64 = i64::MAX as u64;

/// An instant: milliseconds since the Unix epoch, as the caller's clock reads them. The
/// scaler reads no clock; the caller passes one in, so a records file means the same moment
/// after a restart and a test moves time by building a later value. Saturates at the
/// largest instant `js_lease::Timestamp` holds, in the constructor, in arithmetic and when
/// a file is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "u64", into = "u64")]
pub struct Millis(u64);

impl From<u64> for Millis {
    fn from(millis: u64) -> Self {
        Self::new(millis)
    }
}

impl From<Millis> for u64 {
    fn from(instant: Millis) -> Self {
        instant.0
    }
}

impl Millis {
    /// The instant `millis` after the epoch, saturating at the largest instant.
    #[must_use]
    pub fn new(millis: u64) -> Self {
        Self(millis.min(MAX_MILLIS))
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub fn as_millis(self) -> u64 {
        self.0
    }

    /// This instant plus `after`, saturating at the largest instant.
    #[must_use]
    pub fn plus(self, after: Duration) -> Self {
        Self::new(
            self.0
                .saturating_add(u64::try_from(after.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// This instant minus `before`, saturating at the epoch.
    #[must_use]
    pub(crate) fn back(self, before: Duration) -> Self {
        Self(
            self.0
                .saturating_sub(u64::try_from(before.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// How long after `earlier` this instant is; zero if it is not after it.
    #[must_use]
    pub fn since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}
