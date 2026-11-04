//! The stop after a silence of the server (`--silence-stop`): when the agent has heard
//! nothing for the bound, every job it holds that runs, or waits for its image, is
//! stopped as a `Stop` stops it (the stop grace, then the kill; a native run's helper
//! kills at once), its fence is retired, and its result says `killed` with the detail
//! [`detail::SILENCE_STOP`](crate::lifecycle::detail::SILENCE_STOP). The silence counts
//! from the last [`Contact`] the session stamped, or from the moment the watch began when
//! there has been none; it is stopped once for each silence, and a contact ends it.

use std::time::Duration;

use super::Lifecycle;
use super::job::Cause;
use crate::lifecycle::{Contact, Event, code};

/// What the lifecycle keeps to stop its jobs after a silence.
pub(super) struct Watch {
    /// The bound, in milliseconds of the lifecycle's clock.
    bound_ms: u64,
    /// When the server was last heard.
    contact: Contact,
    /// When the watch began, on the clock: the silence counts from here until the first
    /// contact.
    armed_ms: u64,
    /// The start of the silence the jobs were last stopped for: one stop for each.
    stopped_for: Option<u64>,
}

impl Lifecycle {
    /// From now on, stops every job the agent holds once the server has not been heard
    /// for `bound`: [`Contact::last`], or the moment of this call while there has been no
    /// contact, on the lifecycle's clock. A job adopted after a restart is stopped by the
    /// same rule. Without this call nothing stops a job for silence.
    pub fn stop_on_silence(&mut self, bound: Duration, contact: Contact) {
        self.silence = Some(Watch {
            bound_ms: u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
            contact,
            armed_ms: self.clock.now_ms(),
            stopped_for: None,
        });
    }

    /// At every step, before the jobs are watched: when the server has been silent for the
    /// bound, every job that runs or waits for its image is asked to stop for the silence
    /// (a cause already set stays), once for this silence, its fence is retired, and a
    /// finding says so. The stop itself is made when the same step then watches the job, as
    /// for a `Stop`.
    pub(super) fn watch_silence(&mut self, events: &mut Vec<Event>) {
        let now = self.clock.now_ms();
        let Self {
            silence,
            jobs,
            retired,
            ..
        } = self;
        let Some(watch) = silence else {
            return;
        };
        let since = watch.contact.last().unwrap_or(watch.armed_ms);
        let silent = now.saturating_sub(since);
        if silent < watch.bound_ms || watch.stopped_for == Some(since) {
            return;
        }
        watch.stopped_for = Some(since);
        for job in jobs.iter_mut() {
            if job.request_stop_for(Cause::Silenced, false) {
                retired.insert_with(&job.run_id, Some(&job.lease_id));
                let message =
                    format!("the server was not heard for {silent} ms: the job is stopped");
                job.note(code::SILENCE_STOP, message, events);
            }
        }
    }
}
