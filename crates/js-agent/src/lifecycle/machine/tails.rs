//! Keeps the log tails of the held results within what one heartbeat can carry.
//!
//! A result is resent in every heartbeat until the server acknowledges it, and the body
//! the server reads is bounded ([`limits::HEARTBEAT_BODY_BUDGET`]). A tail of plain text
//! is 8 KiB a stream, but a job that prints control characters serialises six times as
//! large (`\u00XX`), so the results are measured as they are sent.

use js_agent_wire::{HeldRun, LogTail, limits};

use super::Lifecycle;
use crate::lifecycle::{Event, code};

/// What a heartbeat keeps for its envelope (ids, session, phase, taint, exclusive fields),
/// in bytes.
const ENVELOPE_MARGIN: usize = 4 * 1024;

/// What the results of a heartbeat may weigh together once serialised, given the held
/// list that goes with them: the whole body the server reads
/// ([`limits::HEARTBEAT_BODY_BUDGET`]; a beat over it is refused and the agent ends) less
/// the held list as it is, the most a load can weigh ([`limits::MAX_LOAD_BYTES`], reserved
/// whether or not this agent sends one yet) and the envelope. Measuring the held list, and
/// not assuming its worst case, leaves the room a host with short ids really has.
fn results_budget(held: &[HeldRun]) -> usize {
    let held = serde_json::to_vec(held).map_or(0, |bytes| bytes.len());
    let rest = held + limits::MAX_LOAD_BYTES + ENVELOPE_MARGIN;
    limits::HEARTBEAT_BODY_BUDGET.saturating_sub(rest)
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
        let budget = results_budget(&self.held());
        self.fit_log_tails(budget, events);
    }

    /// Cuts the log tails of the held results until their results serialise to at most
    /// `budget` bytes together: the largest tail is halved (the end is kept and
    /// `truncated` is set) again and again. A tail that is empty and still does not fit
    /// is left out of its result, and the run notes a `log-tail-shed` finding. The stored
    /// result is changed, so every heartbeat carries the same cut.
    pub fn fit_log_tails(&mut self, budget: usize, events: &mut Vec<Event>) {
        let size = |jobs: &[super::Job]| -> usize {
            jobs.iter()
                .filter_map(|job| job.result.as_ref())
                .map(|result| serde_json::to_vec(result).map_or(0, |bytes| bytes.len()))
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
            if dropped {
                let why = "the log tail did not fit a heartbeat and was left out".to_owned();
                job.note(code::LOG_TAIL_SHED, why, events);
            }
        }
    }
}
