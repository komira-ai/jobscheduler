//! Usage collection: the cgroup readers, the sampler over fake files, and the binner over
//! samples built by hand. The clock is a counter that moves when a test moves it, so no
//! test waits and none asserts a duration.

mod binner;
mod cgroup;
mod collect;
mod outbox;
mod sampler;
mod wiring;

use std::cell::Cell;

use js_agent::usage::UsageClock;

/// A clock that reads what it was set to and counts its reads.
pub struct StepClock {
    now: u64,
    reads: Cell<u32>,
}

impl StepClock {
    /// A clock stopped at `now` microseconds.
    pub fn at(now: u64) -> Self {
        Self {
            now,
            reads: Cell::new(0),
        }
    }

    /// How many times the clock was read.
    pub fn reads(&self) -> u32 {
        self.reads.get()
    }
}

impl UsageClock for StepClock {
    fn now_usec(&self) -> u64 {
        self.reads.set(self.reads.get() + 1);
        self.now
    }
}
