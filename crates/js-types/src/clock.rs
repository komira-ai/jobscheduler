//! Time as an input: a wall-clock [`Timestamp`], the [`Clock`] and [`Monotonic`] traits a
//! caller reads time through, and [`ManualClock`], a clock that moves only when told to.
//!
//! Nothing here reads the system's clocks. The clocks that do, `SystemClock` and
//! `SystemMonotonic`, are in the runtime crate `js-system-clock`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The largest millisecond count a [`Timestamp`] holds. TOML integers are signed 64-bit, and
/// timestamps are written to TOML files, so a timestamp saturates here instead of overflowing
/// into a value such a file cannot hold.
const MAX_MILLIS: u64 = i64::MAX as u64;

/// A wall-clock instant: milliseconds since the Unix epoch.
///
/// Wall time and not a monotonic instant, because an instant that is written to a file must
/// mean the same moment after a restart; a monotonic reading has no meaning in another
/// process. The cost is that a clock stepped backwards stretches a bound computed from it and
/// one stepped forwards cuts it short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The largest instant a timestamp holds, where [`Timestamp::plus`] saturates.
    pub const MAX: Self = Self(MAX_MILLIS);

    /// The instant `millis` milliseconds after the epoch, saturating at the largest
    /// instant a timestamp holds.
    pub fn from_millis(millis: u64) -> Self {
        Self(millis.min(MAX_MILLIS))
    }

    /// Milliseconds since the epoch.
    pub fn millis(&self) -> u64 {
        self.0
    }

    /// This instant plus `after`, saturating at the largest instant.
    pub fn plus(&self, after: Duration) -> Self {
        let after = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        Self::from_millis(self.0.saturating_add(after))
    }
}

/// A source of wall time.
pub trait Clock {
    /// The current wall time.
    fn now(&self) -> Timestamp;
}

/// A source of elapsed time that never runs backwards, for measuring how long a step
/// took. It reads a duration from an origin of its own, so only the difference of two
/// readings means anything.
pub trait Monotonic: Send + Sync + std::fmt::Debug {
    /// The time since this clock's origin.
    fn reading(&self) -> Duration;
}

impl<T: Monotonic + ?Sized> Monotonic for Arc<T> {
    fn reading(&self) -> Duration {
        (**self).reading()
    }
}

/// A clock that moves only when told to. Clones share one reading, so a test keeps
/// a handle, gives the code under test a clone, and advances time through its own handle.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    millis: Arc<AtomicU64>,
}

impl ManualClock {
    /// A clock reading `start`.
    pub fn new(start: Timestamp) -> Self {
        Self {
            millis: Arc::new(AtomicU64::new(start.millis())),
        }
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.set(self.now().plus(by));
    }

    /// Sets the clock to `to`, forwards or back.
    pub fn set(&self, to: Timestamp) {
        self.millis.store(to.millis(), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.millis.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    /// A monotonic clock that reads a fixed duration.
    #[derive(Debug)]
    struct Fixed(Duration);

    impl Monotonic for Fixed {
        fn reading(&self) -> Duration {
            self.0
        }
    }

    /// Asserts that timestamps saturate instead of wrapping, at `MAX` and past it, and that
    /// a sum below the top is exact.
    #[test]
    fn timestamps_saturate_at_the_top() {
        let top = Timestamp::from_millis(u64::MAX);
        assert_eq!(top.millis(), i64::MAX as u64);
        assert_eq!(top, Timestamp::MAX);
        assert_eq!(top.plus(Duration::MAX), top);
        assert_eq!(at(1).plus(Duration::from_millis(u64::MAX)), top);
        assert_eq!(at(5).plus(Duration::from_millis(7)), at(12));
        assert_eq!(at(5).plus(Duration::ZERO), at(5));
    }

    /// Asserts that the manual clock moves only when told to, that clones share one
    /// reading, that it can be set back, and that a default clock reads the epoch.
    #[test]
    fn the_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(at(10));
        let other = clock.clone();
        assert_eq!(clock.now(), at(10));
        other.advance(Duration::from_millis(5));
        assert_eq!(clock.now(), at(15));
        clock.set(at(3));
        assert_eq!(other.now(), at(3));
        assert_eq!(ManualClock::default().now(), at(0));
    }

    /// Asserts that a monotonic clock behind an `Arc` reads what the clock inside reads.
    #[test]
    fn a_shared_monotonic_clock_reads_its_inner_clock() {
        let shared: Arc<dyn Monotonic> = Arc::new(Fixed(Duration::from_millis(42)));
        assert_eq!(Monotonic::reading(&shared), Duration::from_millis(42));
    }
}
