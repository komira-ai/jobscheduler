//! The timeline of each start between its admission and its container (`StartTimeline`):
//! begun when a start is admitted, marked at each step on the lifecycle's clock, and handed
//! to [`Event::Started`](crate::lifecycle::Event::Started) with the runtime's own steps when
//! the container started. A start that ends without one drops its timeline.
//!
//! And the wake the lifecycle hands on to its runtime and its providers
//! ([`Lifecycle::with_wake`]).

use std::collections::BTreeMap;

use super::Lifecycle;
use crate::lifecycle::runtime::RunHandle;
use crate::lifecycle::{StartTimeline, Wake};

/// Marks `step` of the start `run_id` in `starting` as done at `now`, for a caller that
/// holds other parts of the lifecycle borrowed.
pub(super) fn mark_at(
    starting: &mut BTreeMap<String, StartTimeline>,
    run_id: &str,
    step: &'static str,
    now: u64,
) {
    let timeline = starting.entry(run_id.to_owned()).or_default();
    let after = now.saturating_sub(timeline.received_ms);
    timeline.steps.push((step, after));
}

impl Lifecycle {
    /// Gives the runtime, the pool of the cleanup ([`Lifecycle::with_cleanup_threads`]) and the
    /// providers attached from now on ([`Lifecycle::attach_providers`]), `wake` to ring when
    /// work they do off the worker is done: the worker that steps this lifecycle steps it at
    /// once, and a start or a cleanup that waited on that work goes on.
    #[must_use]
    pub fn with_wake(mut self, wake: Wake) -> Self {
        self.runtime.set_wake(wake.clone());
        self.run_dirs.set_wake(&wake);
        self.wake = wake;
        self
    }

    /// Begins the timeline of the start `run_id`, which arrived at `received` and was
    /// taken at `taken` (both on the lifecycle's clock) and is admitted now, and which the
    /// server placed at `placed_at_ms` (Unix time) when it says. Its arrival in Unix time is
    /// the Unix time now less the time since `received`, when the clock has a Unix time.
    pub(super) fn begin_timeline(
        &mut self,
        run_id: &str,
        (received, taken): (u64, u64),
        placed_at_ms: Option<u64>,
    ) {
        let since_received = self.clock.now_ms().saturating_sub(received);
        let timeline = StartTimeline {
            received_ms: received,
            placed_at_ms,
            received_at_ms: self
                .clock
                .unix_ms()
                .map(|now| now.saturating_sub(since_received)),
            steps: vec![("taken", taken.saturating_sub(received))],
            runtime: Vec::new(),
        };
        self.starting.insert(run_id.to_owned(), timeline);
        self.mark(run_id, "admitted");
    }

    /// Marks `step` of the start `run_id` done now. Only a start that was admitted is marked,
    /// and its timeline was begun then.
    pub(super) fn mark(&mut self, run_id: &str, step: &'static str) {
        let now = self.clock.now_ms();
        mark_at(&mut self.starting, run_id, step, now);
    }

    /// The timeline of the start `run_id`, whose container `id` the runtime created, with
    /// the runtime's steps; it is no longer kept.
    pub(super) fn take_timeline(&mut self, run_id: &str, id: &RunHandle) -> StartTimeline {
        let mut timeline = self.starting.remove(run_id).unwrap_or_default();
        timeline.runtime = self.runtime.start_steps(id);
        timeline
    }

    /// Drops the timeline of the start `run_id`, which ended without a container.
    pub(super) fn drop_timeline(&mut self, run_id: &str) {
        self.starting.remove(run_id);
    }
}
