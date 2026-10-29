//! Keeps the log tails of the held results within what one heartbeat can carry.
//!
//! A result is resent in every heartbeat until the server acknowledges it, and the body
//! the server reads is bounded ([`limits::HEARTBEAT_BODY_BUDGET`]). A tail of plain text
//! is 8 KiB a stream, but a job that prints control characters serialises six times as
//! large (`\u00XX`), so the results are measured as they are sent.

use js_agent_wire::{LogTail, limits};

use super::Lifecycle;
use crate::lifecycle::{Event, code};

/// What the results of a heartbeat may weigh together once serialised: three quarters of
/// the body the server reads (as the wire holds its own totals), less 8 KiB for the rest
/// of the beat (the held list and the envelope).
pub(super) const RESULTS_BUDGET: usize = limits::HEARTBEAT_BODY_BUDGET / 4 * 3 - 8 * 1024;

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
            match (weight, job.result.as_mut()) {
                (0, Some(result)) => {
                    result.log_tail = None;
                    let why = "the log tail did not fit a heartbeat and was left out".to_owned();
                    job.note(code::LOG_TAIL_SHED, why, events);
                }
                (_, Some(result)) => result.log_tail.iter_mut().for_each(halve),
                (_, None) => return,
            }
        }
    }
}
