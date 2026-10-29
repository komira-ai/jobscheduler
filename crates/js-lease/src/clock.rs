use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The largest millisecond count a [`Timestamp`] holds. The persisted file is TOML,
/// whose integers are signed 64-bit, so a timestamp saturates here instead of
/// overflowing into a value the file cannot hold.
const MAX_MILLIS: u64 = i64::MAX as u64;

/// A wall-clock instant: milliseconds since the Unix epoch.
///
/// The lease table uses wall time and not a monotonic instant because an expiry is
/// written to the file and must mean the same moment after a restart; a monotonic
/// reading has no meaning in another process. The cost is that a clock stepped
/// backwards stretches a lease and one stepped forwards cuts it short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
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

/// A source of wall time. The table reads it and nothing else does.
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

/// The system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
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
    fn reading(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that moves only when told to. Clones share one reading, so a test keeps
/// a handle, gives the table a clone, and advances time through its own handle.
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
