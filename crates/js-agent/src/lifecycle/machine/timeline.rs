//! The timeline of each start between its admission and its container (`StartTimeline`):
//! begun when a start is admitted, marked at each step on the lifecycle's clock, and handed
//! to [`Event::Started`](crate::lifecycle::Event::Started) with the runtime's own steps when
//! the container started. A start that ends without one drops its timeline.
//!
//! And the wake the lifecycle hands on to its runtime and its providers
//! ([`Lifecycle::with_wake`]).

use super::Lifecycle;
use crate::lifecycle::runtime::RunHandle;
use crate::lifecycle::{StartTimeline, Wake};

impl Lifecycle {
    /// Gives the runtime, and the providers attached from now on
    /// ([`Lifecycle::attach_providers`]), `wake` to ring when work they do off the worker is
    /// done: the worker that steps this lifecycle steps it at once, and a start that waited
    /// on that work goes on.
    #[must_use]
    pub fn with_wake(mut self, wake: Wake) -> Self {
        self.runtime.set_wake(wake.clone());
        self.wake = wake;
        self
    }

    /// Begins the timeline of the start `run_id`, which arrived at `received` and was
    /// taken at `taken` and is admitted now.
    pub(super) fn begin_timeline(&mut self, run_id: &str, received: u64, taken: u64) {
        let timeline = StartTimeline {
            received_ms: received,
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
        let timeline = self.starting.entry(run_id.to_owned()).or_default();
        let after = now.saturating_sub(timeline.received_ms);
        timeline.steps.push((step, after));
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
