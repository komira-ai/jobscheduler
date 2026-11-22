//! The clocks the lifecycle's tests move by hand: nothing here reads the time.

use std::cell::Cell;
use std::rc::Rc;

use js_agent::lifecycle::Clock;

/// A clock that moves only when a test says so. Clones share one reading.
#[derive(Clone, Default)]
pub struct ManualClock(Rc<Cell<u64>>);

impl ManualClock {
    pub fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms);
    }
}

/// The Unix time in milliseconds at which a [`ManualClock`] reads zero.
pub const UNIX_ORIGIN_MS: u64 = 1_800_000_000_000;

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }

    fn unix_ms(&self) -> Option<u64> {
        Some(UNIX_ORIGIN_MS + self.0.get())
    }
}

/// A [`ManualClock`] that has no Unix time.
pub struct NoUnixClock(pub ManualClock);

impl Clock for NoUnixClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}
