//! The fake cloud's clock: a number the test moves.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Milliseconds in a duration, saturating.
pub(crate) fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

/// A clock that moves only when told to. Clones share one time, so the test and the
/// fake cloud read the same instant. The cloud keeps its bill from this clock and from
/// nothing else: it never reads the system clock.
#[derive(Debug, Clone)]
pub struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    /// A clock that reads `start_ms` milliseconds.
    pub fn new(start_ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(start_ms)))
    }

    /// The time now, in milliseconds.
    pub fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(millis(by), Ordering::SeqCst);
    }
}
