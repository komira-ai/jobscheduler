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

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the time since a stamp taken now is never before the stamp: the
    /// monotonic clock does not run back. Catches a stamp read from a wall clock.
    #[test]
    fn the_clock_does_not_run_back() {
        let began = now();
        assert!(now() >= began);
        let _ = micros_since(began);
    }
}
