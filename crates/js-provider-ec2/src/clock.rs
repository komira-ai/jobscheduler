//! Time as the provider's guards read it. A clock is injected so that no test waits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A monotonic clock: the time since some origin that never changes.
pub trait Clock: Send + Sync {
    /// The time since the clock's origin. It never goes back.
    fn now(&self) -> Duration;
}

/// The operating system's monotonic clock, with its origin at construction.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that moves only when told to, for tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    /// A clock at its origin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.millis.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.millis.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the manual clock starts at zero, stands still until told, adds what it
    /// is told, and takes a span too long to count as the largest count it holds.
    #[test]
    fn the_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        assert_eq!(clock.now(), Duration::ZERO);
        clock.advance(Duration::from_millis(1500));
        assert_eq!(clock.now(), Duration::from_millis(1500));
        let huge = ManualClock::new();
        huge.advance(Duration::MAX);
        assert_eq!(huge.now(), Duration::from_millis(u64::MAX));
    }

    /// Asserts that the system clock, built both ways, never reads an earlier time than
    /// it read before.
    #[test]
    fn the_system_clock_never_goes_back() {
        for clock in [SystemClock::new(), SystemClock::default()] {
            let first = clock.now();
            assert!(clock.now() >= first);
        }
    }
}
