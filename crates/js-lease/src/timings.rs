//! What the table times: how long each step took, kept per operation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tracing::warn;

use crate::clock::Monotonic;

/// A step that takes longer than this is logged as slow. A step of exactly this long
/// is not.
const SLOW: Duration = Duration::from_secs(1);

/// Times steps on a monotonic clock and keeps, per operation, how many it timed, how
/// many took over a second and the longest. A step over a second is also logged as a
/// warning naming the operation and the milliseconds. Clones share one record.
#[derive(Debug, Clone)]
pub struct Timings(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    clock: Box<dyn Monotonic>,
    seen: Mutex<BTreeMap<&'static str, Seen>>,
}

/// What was timed under one operation.
#[derive(Debug, Default)]
struct Seen {
    timed: u64,
    slow: u64,
    longest: Duration,
}

/// A step being timed; [`Timer::finish`] records it.
#[derive(Debug)]
pub struct Timer {
    timings: Timings,
    operation: &'static str,
    began: Duration,
}

impl Timings {
    /// A record that times on `clock`.
    pub fn new(clock: impl Monotonic + 'static) -> Self {
        Self(Arc::new(Inner {
            clock: Box::new(clock),
            seen: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Starts timing a step of the table called `operation`.
    pub fn begin(&self, operation: &'static str) -> Timer {
        Timer {
            timings: self.clone(),
            operation,
            began: self.0.clock.reading(),
        }
    }

    /// What was timed, one operation after another in name order, for example
    /// `renew: 3 timed, 1 over 1 s, longest 1500 ms`; `no lease steps timed` when
    /// nothing was.
    pub fn report(&self) -> String {
        let seen = self.0.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.is_empty() {
            return "no lease steps timed".to_string();
        }
        let line = |(operation, seen): (&&str, &Seen)| {
            let (timed, slow) = (seen.timed, seen.slow);
            let longest = seen.longest.as_millis();
            format!("{operation}: {timed} timed, {slow} over 1 s, longest {longest} ms")
        };
        seen.iter().map(line).collect::<Vec<_>>().join("; ")
    }
}

impl Timer {
    /// Ends the step and records how long it took; a step over a second is logged.
    pub fn finish(self) {
        let took = self.timings.0.clock.reading().saturating_sub(self.began);
        let slow = took > SLOW;
        if slow {
            let (operation, took_ms) = (self.operation, took.as_millis());
            warn!(%operation, took_ms, "slow lease step");
        }
        let mut all = self
            .timings
            .0
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let seen = all.entry(self.operation).or_default();
        seen.timed += 1;
        seen.slow += u64::from(slow);
        seen.longest = seen.longest.max(took);
    }
}
