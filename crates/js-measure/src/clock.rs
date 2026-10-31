//! The time the driver reads and waits on, as a seam: a run against a fake farm moves a
//! fake clock instead of sleeping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A clock the driver can read and wait on.
pub trait Clock {
    /// Milliseconds since the Unix epoch. The harness buckets usage minutes with this
    /// clock while the lease server buckets its rows with its own, so the two are assumed
    /// to be in sync; skew moves usage into a neighbouring minute.
    fn now_ms(&self) -> u64;
    /// Waits `ms` milliseconds.
    fn sleep_ms(&self, ms: u64);
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
