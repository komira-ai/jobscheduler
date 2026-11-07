//! The system's clocks behind the traits of `js-types`: [`SystemClock`], the wall clock, as a
//! [`js_types::Clock`], and [`SystemMonotonic`], the monotonic clock, as a
//! [`js_types::Monotonic`].
//!
//! A pure crate takes time as an input and reads neither; a server or a node gives it one of
//! these, and a test gives it a [`js_types::ManualClock`] or a monotonic clock of its own.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use js_types::{Clock, Monotonic, Timestamp};

/// The system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    /// The milliseconds since the Unix epoch, or the epoch itself for a system clock set
    /// before it.
    fn now(&self) -> Timestamp {
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
        Timestamp::from_millis(0).plus(since_epoch.unwrap_or_default())
    }
}

/// The system's monotonic clock: the time since it was made.
#[derive(Debug, Clone, Copy)]
pub struct SystemMonotonic {
    origin: Instant,
}

impl Default for SystemMonotonic {
    /// A clock whose origin is now.
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Monotonic for SystemMonotonic {
    fn reading(&self) -> std::time::Duration {
        self.origin.elapsed()
    }
}
