//! Keeps everything a heartbeat lists within the body the server reads.
//!
//! The server reads at most [`limits::HEARTBEAT_BODY_BUDGET`] bytes of a heartbeat and
//! answers a longer one 413 `body-too-large`, which the agent can only retry. The body holds an
//! envelope (ids, session, phase, taint, exclusive fields), a load, an entry for each
//! run or job the agent holds, the results it has not had acknowledged, and the job ids
//! retired on a stop for a job it did not hold. Each part is bounded here:
//!
//! - the envelope by [`ENVELOPE_MARGIN`] and the load by [`limits::MAX_LOAD_BYTES`],
//!   reserved whether or not this agent sends a load;
//! - the held entries wherever a job is added: a start is taken, a container taken over and
//!   a result read back at a restart only while every held entry, at its longest state,
//!   fits [`LISTED_BUDGET`] ([`Lifecycle::listable`], [`Lifecycle::listing_room`]), so the
//!   entries always leave room for the largest result without its log tail
//!   ([`RESULT_RESERVE`]);
//! - the retired job ids, oldest first, in what the entries leave (all of them, but for
//!   a host whose entries are near their bound and whose server has not recorded many
//!   stops); an id left out waits for a later heartbeat, and the server sends its stop
//!   again meanwhile. They go before the results because the first heartbeat after a hello
//!   loses a job that it neither lists nor names retired, where the server would have
//!   ended it as stopped;
//! - the results, oldest first, as long as they fit what is left; a result that does not,
//!   and every one after it, waits for a later heartbeat. Its job is listed all the same,
//!   without its result, so the server neither loses the job nor starts it again, and only
//!   the job's end is reported later ([`Lifecycle::fit`]). With no retired id waiting for
//!   the server, the oldest result always goes out.
//!
//! The results' log tails are cut before any result waits (`tails`). An entry is never
//! left out: the first heartbeat after a hello loses a job it does not name.

use js_agent_wire::{HeldRun, HeldState, limits};

use super::Lifecycle;
use super::job::Job;
use super::tails::{len_of, result_len};

/// What a heartbeat keeps for its envelope (ids, session, phase, taint, exclusive fields),
/// in bytes.
pub(super) const ENVELOPE_MARGIN: usize = 4 * 1024;

/// The most one result without its log tail weighs in a heartbeat, in bytes, with the key
/// or comma that goes with it: a job's at the longest ids, a `detail` and `input_failure`
/// whose every character is escaped, and full usage and series (a test builds one and holds
/// it to this).
pub(super) const RESULT_RESERVE: usize = 8 * 1024;

/// What the held entries, the results and the retired job ids of a heartbeat may weigh
/// together: the body less the load and the envelope.
const LIST_ROOM: usize = limits::HEARTBEAT_BODY_BUDGET - limits::MAX_LOAD_BYTES - ENVELOPE_MARGIN;

/// What the held entries may weigh together, each at its longest state: what a heartbeat
/// lists less one result at its largest, so the oldest result always goes out.
pub(super) const LISTED_BUDGET: usize = LIST_ROOM - RESULT_RESERVE;

/// The least an entry weighs: one at a one-byte id, `{"run_id":"x","state":"exited_unacked"}`
/// and its comma.
const SHORTEST_ENTRY: usize = 40;
const _: () = assert!(
    LISTED_BUDGET / SHORTEST_ENTRY <= super::MAX_JOBS,
    "the entries a start is held to stay within the count the wire allows"
);

/// The most the entry of a run or job held under `id` weighs in a heartbeat, with its
/// comma: `{"run_id":"…","state":"exited_unacked"}`, the longest state. A job's entry
/// (`job_id`) without its result is as long.
pub(super) fn entry_len(id: &str) -> usize {
    let entry = HeldRun {
        run_id: id.to_owned(),
        state: HeldState::ExitedUnacked,
    };
    len_of(&entry) + 1
}

/// What the entries of `jobs` weigh together, each at its longest state.
fn listed_len(jobs: &[Job]) -> usize {
    jobs.iter().map(|job| entry_len(&job.run_id)).sum()
}

/// What the next heartbeat carries besides the held entries.
pub(super) struct Fit {
    /// For each held job in order, whether its result goes out.
    pub(super) carried: Vec<bool>,
    /// How many of the retired job ids go out, oldest first.
    pub(super) retired: usize,
}

impl Lifecycle {
    /// What the results of the next heartbeat may weigh together, each with its comma, for
    /// all of them to go out: what a heartbeat lists less the held entries at their longest
    /// state and the retired job ids. The log tails are cut to it (`tails`); measuring the
    /// entries and ids as they are, and not at their caps, leaves the room a host with short
    /// ids really has.
    pub(super) fn results_room(&self) -> usize {
        let listed = listed_len(&self.jobs) + len_of(&self.answered);
        LIST_ROOM.saturating_sub(listed)
    }

    /// The bytes the entries of jobs added from here may take: [`LISTED_BUDGET`] less the
    /// entries held. A result read back at a restart is held to it.
    pub(super) fn listing_room(&self) -> usize {
        LISTED_BUDGET.saturating_sub(listed_len(&self.jobs))
    }

    /// Whether one more entry, held under `id`, still lets every entry fit
    /// [`LISTED_BUDGET`]. A start that does not is skipped (`Skip::Full`), and the server
    /// sends it again until results are acknowledged; a container to take over that does
    /// not is stopped and removed.
    pub(super) fn listable(&self, id: &str) -> bool {
        entry_len(id) <= self.listing_room()
    }

    /// The retired job ids and results the next heartbeat carries: as many retired ids,
    /// oldest first, as fit what the entries leave, then the results oldest first up to the
    /// first that does not fit what is left. The results' tails were cut at the last step to
    /// leave room for them all, so a result waits only when no tail is left to cut.
    pub(super) fn fit(&self) -> Fit {
        let mut room = LIST_ROOM.saturating_sub(listed_len(&self.jobs));
        let retired = self
            .answered
            .iter()
            .take_while(|id| {
                let weight = id.len() + 3;
                let fits = weight <= room;
                room = room.saturating_sub(weight);
                fits
            })
            .count();
        let mut open = true;
        let carried = self
            .jobs
            .iter()
            .map(|job| {
                let weight = job
                    .result
                    .as_ref()
                    .map(|result| result_len(job, result) + 1);
                let Some(weight) = weight else {
                    return false;
                };
                open = open && weight <= room;
                if open {
                    room -= weight;
                }
                open
            })
            .collect();
        Fit { carried, retired }
    }
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
