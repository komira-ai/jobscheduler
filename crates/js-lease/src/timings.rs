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
    /// Steps begun and neither finished nor dropped.
    in_flight: u64,
}

/// A step being timed: in flight until [`Timer::finish`] records it or the timer is
/// dropped (a cancelled future drops it), either of which ends the flight.
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
    ///
    /// `operation` is a name from a small fixed set; the record keeps one entry per
    /// name for the life of the process, so a name built from request data would grow
    /// it without bound.
    pub fn begin(&self, operation: &'static str) -> Timer {
        self.update(operation, |seen| seen.in_flight += 1);
        Timer {
            timings: self.clone(),
            operation,
            began: self.0.clock.reading(),
        }
    }

    fn update(&self, operation: &'static str, change: impl FnOnce(&mut Seen)) {
        let mut all = self.0.seen.lock().unwrap_or_else(PoisonError::into_inner);
        change(all.entry(operation).or_default());
    }

    /// What was timed, one operation after another in name order, for example
    /// `renew: 3 timed, 1 over 1 s, longest 1500 ms`; `no lease steps timed` when
    /// nothing was. A step that has begun and not ended is counted as `in flight`, shown
    /// only while there is one.
    pub fn report(&self) -> String {
        let seen = self.0.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let line = |(operation, seen): (&&str, &Seen)| {
            let (timed, slow) = (seen.timed, seen.slow);
            let longest = seen.longest.as_millis();
            let mut text =
                format!("{operation}: {timed} timed, {slow} over 1 s, longest {longest} ms");
            if seen.in_flight > 0 {
                text.push_str(&format!(", {} in flight", seen.in_flight));
            }
            text
        };
        let lines: Vec<String> = (seen.iter())
            .filter(|(_, seen)| seen.timed + seen.in_flight > 0)
            .map(line)
            .collect();
        match lines.is_empty() {
            true => "no lease steps timed".to_string(),
            false => lines.join("; "),
        }
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
        self.timings.update(self.operation, |seen| {
            seen.timed += 1;
            seen.slow += u64::from(slow);
            seen.longest = seen.longest.max(took);
        });
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.timings
            .update(self.operation, |seen| seen.in_flight -= 1);
    }
}
