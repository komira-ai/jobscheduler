//! What the agent does after a silence of the server (`--silence-stop`): once it has heard
//! nothing for the bound it stops starting work and keeps the work that runs.
//!
//! - Every job it holds that waits to start (it took the start; no container runs yet) is
//!   ended as a `Stop` ends one that waits: nothing is made, its fence is retired, and its
//!   result says `failed_to_start` with the detail
//!   [`detail::SILENCE_STOP`](crate::lifecycle::detail::SILENCE_STOP). Such a job is the one
//!   kind a server can end while the host lives: the start timeout of a placed or starting job
//!   (the lease server's `job_expiry`) ends the attempt, and may hand the job to another attempt
//!   once this host lets it go. The server reads the code as the host's (a refusal with a code
//!   that names no fault of the job), so the job's retry policy may place it again.
//! - Every start that reaches the lifecycle while the silence lasts is refused with the same
//!   code (see `Lifecycle::admit`), so nothing new begins on a host whose reports reach no
//!   server.
//! - A job whose container create is under way (the phase `Creating`: the runtime makes and
//!   starts it off the worker, and cannot be told to stop short of it) counts as running:
//!   its container is made, runs and is reported `running`.
//! - A job whose container runs is left alone: it runs to its own limits (its wall limit, a
//!   stop or cancel the server sends once it is heard again), and its result waits in the
//!   state root until the server acknowledges it. No rule of the server ends or re-places a
//!   running job for its host's silence (the lease server's `job_loss`, "Silence ends
//!   nothing"): stopping it would lose the work and protect nothing.
//!
//! The silence counts from the last [`Contact`] the session stamped, or from the moment the
//! watch began when there has been none; the waiting jobs are ended once for each silence,
//! and a contact ends it.

use std::time::Duration;

use super::Lifecycle;
use super::job::{Cause, Phase, Stage};
use crate::lifecycle::{Contact, Event, code};

/// What the lifecycle keeps to stop starting work after a silence.
pub(super) struct Watch {
    /// The bound, in milliseconds of the lifecycle's clock.
    bound_ms: u64,
    /// When the server was last heard.
    contact: Contact,
    /// When the watch began, on the clock: the silence counts from here until the first
    /// contact.
    armed_ms: u64,
    /// The start of the silence the waiting jobs were last ended for: once for each.
    stopped_for: Option<u64>,
}

impl Watch {
    /// How long the server has been silent at `now_ms`, and since when.
    fn silence_at(&self, now_ms: u64) -> (u64, u64) {
        let since = self.contact.last().unwrap_or(self.armed_ms);
        (now_ms.saturating_sub(since), since)
    }
}

impl Lifecycle {
    /// From now on, stops starting work once the server has not been heard for `bound`:
    /// [`Contact::last`], or the moment of this call while there has been no contact, on the
    /// lifecycle's clock (see the module documentation). A job adopted after a restart is
    /// judged by the same rule. Without this call a silence changes nothing.
    pub fn stop_on_silence(&mut self, bound: Duration, contact: Contact) {
        self.silence = Some(Watch {
            bound_ms: u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
            contact,
            armed_ms: self.clock.now_ms(),
            stopped_for: None,
        });
    }

    /// How long the server has been silent, when that is at least the bound; none while it
    /// is heard within the bound and for a lifecycle that watches no silence.
    pub(super) fn silenced(&self) -> Option<u64> {
        let watch = self.silence.as_ref()?;
        let (silent, _) = watch.silence_at(self.clock.now_ms());
        (silent >= watch.bound_ms).then_some(silent)
    }

    /// At every step, before the jobs are watched: when the server has been silent for the
    /// bound, every job that waits to start is asked to end for the silence (a cause already
    /// set stays), once for this silence, its fence is retired, and a finding says so. The
    /// end itself is made when the same step then advances the job, as for a `Stop`. A job
    /// whose container runs, or whose create is under way, is not touched.
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
        let (silent, since) = watch.silence_at(now);
        if silent < watch.bound_ms || watch.stopped_for == Some(since) {
            return;
        }
        watch.stopped_for = Some(since);
        let waiting = jobs.iter_mut().filter(|job| match &job.stage {
            Stage::Preparing(waiting) => waiting.phase != Phase::Creating,
            _ => false,
        });
        for job in waiting {
            if job.request_stop_for(Cause::Silenced, false) {
                retired.insert_with(&job.run_id, Some(&job.lease_id));
                let message = format!(
                    "the server was not heard for {silent} ms: the job, which had not started, \
                     is not started"
                );
                job.note(code::SILENCE_STOP, message, events);
            }
        }
    }
}
