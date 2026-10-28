//! The clock the usage module reads.

use std::time::Instant;

/// One minute in microseconds.
pub const MINUTE_USEC: u64 = 60_000_000;

/// A source of monotonic time in microseconds.
///
/// The contract: the reading is monotonic and never wall time, it never goes back, and a
/// step of the machine's clock (NTP, an operator) cannot move it.
pub trait UsageClock {
    /// Microseconds since some fixed point of this clock.
    fn now_usec(&self) -> u64;
}

/// The production clock: microseconds since it was made, from the operating system's
/// monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    /// A clock that reads 0 now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageClock for MonotonicClock {
    fn now_usec(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}
