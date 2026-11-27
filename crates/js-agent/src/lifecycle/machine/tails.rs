//! Keeps the log tails of the held results within what one heartbeat can carry.
//!
//! A result is resent in every heartbeat until the server acknowledges it, and the body
//! the server reads is bounded ([`js_agent_wire::limits::HEARTBEAT_BODY_BUDGET`]). A tail of plain text
//! is 8 KiB a stream, but a job that prints control characters serialises six times as
//! large (`\u00XX`), so the results are measured as they are sent. A tail cut down notes
//! a `log-tail-cut` finding and one left out a `log-tail-shed` finding, once each for a
//! run, so the agent's log counts the runs whose tails a heartbeat could not carry whole.
//! What a heartbeat carries once the tails are cut is `listing`'s.

use js_agent_wire::{LogTail, RunResult};

use super::job::Keyed;
use super::{Job, Lifecycle, per_job};
use crate::lifecycle::{Event, code};

/// The bytes the key that holds a job's result in its listing adds to it (`,"result":`).
const RESULT_KEY_LEN: usize = 10;

/// The serialised length of `value`.
pub(super) fn len_of<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// What the result of `job` weighs in a heartbeat: a run's in `results`, a job's inside its
/// listing, in the form each is sent, without the comma after it.
pub(super) fn result_len(job: &Job, result: &RunResult) -> usize {
    match job.keyed {
        Keyed::Run => len_of(result),
        Keyed::Job => {
            len_of(&per_job::job_result(
                result,
                job.logs,
                job.input_failure.clone(),
                job.usage,
                (job.cpu_series.clone(), job.memory_series.clone()),
            )) + RESULT_KEY_LEN
        }
    }
}

/// `text` cut to at most `max` bytes, keeping the end, at a character boundary.
fn keep_end(text: &mut String, max: usize) {
    if text.len() > max {
        let mut at = text.len() - max;
        while !text.is_char_boundary(at) {
            at += 1;
        }
        text.drain(..at);
    }
}

/// Half of each stream, the end kept, and the tail marked as cut.
fn halve(tail: &mut LogTail) {
    let (out, err) = (tail.stdout.len() / 2, tail.stderr.len() / 2);
    keep_end(&mut tail.stdout, out);
    keep_end(&mut tail.stderr, err);
    tail.truncated = true;
}

fn weight(tail: &LogTail) -> usize {
    tail.stdout.len() + tail.stderr.len()
}

impl Lifecycle {
    /// [`Lifecycle::fit_log_tails`] to what the next heartbeat can carry.
    pub(super) fn fit_log_tails_to_beat(&mut self, events: &mut Vec<Event>) {
        self.fit_log_tails(self.results_room(), events);
    }

    /// Cuts the log tails of the held results until their results serialise to at most
    /// `budget` bytes together, a comma after each: the largest tail is halved (the end is kept and
    /// `truncated` is set) again and again, and the run notes a `log-tail-cut` finding. A
    /// tail that is empty and still does not fit is left out of its result, and the run
    /// notes a `log-tail-shed` finding. The stored
    /// result is changed, so every heartbeat carries the same cut.
    pub fn fit_log_tails(&mut self, budget: usize, events: &mut Vec<Event>) {
        let size = |jobs: &[Job]| -> usize {
            jobs.iter()
                .filter_map(|job| Some(result_len(job, job.result.as_ref()?) + 1))
                .sum()
        };
        while size(&self.jobs) > budget {
            let heaviest = self
                .jobs
                .iter_mut()
                .filter_map(|job| {
                    let weight = job.result.as_ref()?.log_tail.as_ref().map(weight)?;
                    Some((weight, job))
                })
                .max_by_key(|(weight, _)| *weight);
            let Some((weight, job)) = heaviest else {
                return;
            };
            let dropped = weight == 0;
            // The result is there: the heaviest was found among the jobs that have one.
            job.result.iter_mut().for_each(|result| match dropped {
                true => result.log_tail = None,
                false => result.log_tail.iter_mut().for_each(halve),
            });
            let (code, why) = match dropped {
                true => (code::LOG_TAIL_SHED, "was left out"),
                false => (code::LOG_TAIL_CUT, "was cut down, its end kept"),
            };
            let why = format!("the log tail did not fit a heartbeat and {why}");
            job.note(code, why, events);
        }
    }
}
