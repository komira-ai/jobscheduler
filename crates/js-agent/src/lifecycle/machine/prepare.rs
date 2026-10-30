//! A start that waits for its image: each step asks the runtime again, and the container
//! is created when the image is here.

use js_agent_wire::Outcome;

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
        let Stage::Preparing(waiting) = self.jobs[index].stage.clone() else {
            return;
        };
        let Waiting {
            spec,
            mounts,
            slot,
            booked,
        } = *waiting;
        if self.jobs[index].cause.is_some() {
            self.end_unstarted(index, events);
            return;
        }
        match self.runtime.prepare(&spec.image) {
            Prepared::Pending => {}
            Prepared::Ready => {
                let started = self.begin(&spec, mounts, slot, booked, events);
                self.settle(&spec, started, (slot, booked), Some(index), events);
            }
            Prepared::Failed(why) => {
                let failure = runtime_failure(&why);
                self.settle(&spec, Err(failure), (slot, booked), Some(index), events);
            }
        }
    }

    /// A job stopped before its container was made: killed, reporting nothing when it was
    /// cancelled. Nothing was created, so the cleanup that follows only gives the slot back.
    fn end_unstarted(&mut self, index: usize, events: &mut Vec<Event>) {
        let job = &mut self.jobs[index];
        if !job.silent {
            job.result = Some(job.report(Outcome::Killed, None, None));
            events.push(Event::ResultReady {
                run_id: job.run_id.clone(),
            });
        }
        job.stage = Stage::Scratch;
    }
}
