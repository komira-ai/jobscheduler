//! A start that waits for its image: each step asks the runtime again, and the container
//! is created when the image is here.

use super::job::{Stage, Waiting};
use super::{Lifecycle, runtime_failure};
use crate::lifecycle::Event;
use crate::lifecycle::runtime::Prepared;

impl Lifecycle {
    /// Takes the job at `index`, if it waits for its image, one step on: stopped or
    /// cancelled while it waited, it ends without a container; with the image here it is
    /// created and its wall-time limit begins; an image that could not be had ends it as
    /// `failed_to_start`; otherwise it waits on.
    pub(super) fn advance(&mut self, index: usize, events: &mut Vec<Event>) {
        let job = &mut self.jobs[index];
        let Stage::Preparing(waiting) = &mut job.stage else {
            return;
        };
        if job.cause.is_some() {
            job.end_unstarted(events);
            return;
        }
        match self
            .runtime
            .prepare(&waiting.spec.image, Some(waiting.ticket))
        {
            // The runtime may have started another fetch since (the image was forgotten):
            // the run waits on that one.
            Prepared::Pending(ticket) => waiting.ticket = ticket,
            Prepared::Ready => {
                let waiting = (**waiting).clone();
                let Waiting {
                    spec,
                    mounts,
                    slot,
                    booked,
                    lease_end_ms,
                    ..
                } = waiting;
                let started = self.begin(&spec, mounts, slot, booked, events);
                let at = (Some(index), lease_end_ms);
                self.settle(&spec, started, (slot, booked), at, events);
            }
            Prepared::Failed(why) => {
                let (spec, slot, booked, lease_end_ms) = (
                    waiting.spec.clone(),
                    waiting.slot,
                    waiting.booked,
                    waiting.lease_end_ms,
                );
                let failure = runtime_failure(&why);
                let at = (Some(index), lease_end_ms);
                self.settle(&spec, Err(failure), (slot, booked), at, events);
            }
        }
    }
}
