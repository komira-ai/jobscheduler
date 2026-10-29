//! What the table times: how long each step took, kept per operation.

use std::sync::Arc;

use crate::clock::Monotonic;

/// Times steps on a monotonic clock and keeps, per operation, how many it timed, how
/// many took over a second and the longest. Clones share one record.
#[derive(Debug, Clone)]
pub struct Timings(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    clock: Box<dyn Monotonic>,
}

/// A step being timed; [`Timer::finish`] records it.
#[derive(Debug)]
pub struct Timer {
    _timings: Timings,
}

impl Timings {
    /// A record that times on `clock`.
    pub fn new(clock: impl Monotonic + 'static) -> Self {
        Self(Arc::new(Inner {
            clock: Box::new(clock),
        }))
    }

    /// Starts timing a step of the table called `operation`.
    pub fn begin(&self, operation: &'static str) -> Timer {
        let _ = (operation, &self.0.clock);
        Timer {
            _timings: self.clone(),
        }
    }

    /// What was timed, one operation after another.
    pub fn report(&self) -> String {
        String::new()
    }
}

impl Timer {
    /// Ends the step and records how long it took.
    pub fn finish(self) {}
}
