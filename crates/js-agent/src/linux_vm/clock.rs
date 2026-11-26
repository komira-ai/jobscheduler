//! The one clock the verb reads: the monotonic clock, for its own measurements.

use std::time::Instant;

/// Now, on the monotonic clock.
#[must_use]
pub fn now() -> Instant {
    Instant::now()
}

/// The whole microseconds since `began`.
#[must_use]
pub fn micros_since(began: Instant) -> u64 {
    u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX)
}
