//! Jobs in the cell: the client that submits them ([`JobFront`]), and what the cell
//! observes of them, from the server's job and allocation tables and from what the per-job
//! agents sent and were sent.
//!
//! A job is the design's one public concept: a client submits it, the server queues,
//! places and runs it, and the client reads its result. An allocation is the server's
//! private record that resources on one host are booked for one placement of one job;
//! every placement has its own [`AllocationId`].
//!
//! # What is observed
//!
//! Four sources, each kept apart as [`crate::observe`] keeps its own:
//!
//! - the server's tables, read through the in-process handle
//!   (`InProcess::jobs`) each time the cell reads the server's listing: every change of a
//!   job's state ([`JobRead`]) and of an allocation's row ([`AllocationRead`]), appended
//!   in the order read, and when they were last read ([`crate::Observations::tables_at`]);
//! - each heartbeat a per-job agent sent ([`JobBeat`]): the jobs it listed, with or without
//!   a result, and the ids it listed as retired;
//! - each answer such a heartbeat got ([`JobReply`]): the starts, stops and verdicts it
//!   carried, beside what the heartbeat had listed;
//! - each result an agent made ([`ResultMade`]): a job that ended by itself, or that a
//!   stop ended while it ran.
//!
//! The agents' records are written into the cell's ledger, which outlives an agent
//! process; the server's tables are the server's word, read only where an invariant says
//! so. What each agent runs now is in its ground truth ([`crate::AgentTruth::jobs`]).

use std::collections::BTreeSet;

use js_jobs::{AllocationId, AllocationStep, EndReason, JobId, JobState};
use js_types::HostId;
use js_types::sched::FarmTime;

use crate::observe::{Observations, Verdict};

mod front;
pub(crate) mod read;

pub use front::{JOB_PREFIX, JobFront, LostAnswer};

/// A job's state as one read of the server's job table showed it, when it differed from
/// the read before (or the job was not read before).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRead {
    /// The job.
    pub job: JobId,
    /// Its state.
    pub state: JobState,
    /// When the table was read.
    pub at: FarmTime,
}

/// One row of the server's allocation table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationRow {
    /// The job the allocation books for.
    pub job: JobId,
    /// The allocation: one placement of the job.
    pub allocation: AllocationId,
    /// The host it books on.
    pub host: HostId,
    /// Booked, releasing or freed.
    pub step: AllocationStep,
    /// When the server committed it, on the cell's virtual clock.
    pub created_at: FarmTime,
    /// Why it was freed, once it is.
    pub end_reason: Option<EndReason>,
}

/// An allocation's row as one read of the server's table showed it, when it differed from
/// the read before (or the allocation was not read before).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationRead {
    /// The row.
    pub row: AllocationRow,
    /// When the table was read.
    pub at: FarmTime,
}

/// One heartbeat a per-job agent sent: what it listed, whether or not the server heard it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobBeat {
    /// The host whose agent sent it.
    pub host: HostId,
    /// When it was sent.
    pub at: FarmTime,
    /// The jobs it listed without a result (starting or running).
    pub running: BTreeSet<JobId>,
    /// The jobs it listed with their result.
    pub results: BTreeSet<JobId>,
    /// The ids it listed as retired.
    pub retired: BTreeSet<JobId>,
}

/// A start a per-job agent was sent: the job and the allocation it names, and whether the
/// agent refused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStartSeen {
    /// The job.
    pub job: JobId,
    /// The allocation the start names.
    pub allocation: AllocationId,
    /// The agent refused the start because a stop had named the job before it (a start
    /// after its stop): it launched nothing and does not hold the job.
    pub refused: bool,
}

/// One answer a per-job agent read to one of its heartbeats, with what that heartbeat had
/// listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobReply {
    /// The host whose agent read it.
    pub host: HostId,
    /// When the agent read it.
    pub at: FarmTime,
    /// When the heartbeat it answers was sent.
    pub beat_at: FarmTime,
    /// The jobs that heartbeat listed without a result.
    pub listed: BTreeSet<JobId>,
    /// The starts it carried, in order.
    pub starts: Vec<JobStartSeen>,
    /// The jobs it carried a stop of.
    pub stops: BTreeSet<JobId>,
    /// The verdicts it carried on results, each one this version reads.
    pub acks: Vec<(JobId, Verdict)>,
}

/// A result a per-job agent made: the job ended by itself, or a stop ended it while it
/// ran. The agent sends it in every heartbeat until an answer acknowledges it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultMade {
    /// The host whose agent made it.
    pub host: HostId,
    /// The job.
    pub job: JobId,
    /// When.
    pub at: FarmTime,
}

impl Observations {
    /// These observations with the server's job table read at `at` showing `job` in
    /// `state`, after the reads before.
    #[must_use]
    pub fn with_job_read(mut self, job: JobId, state: JobState, at: FarmTime) -> Self {
        self.job_reads.push(JobRead { job, state, at });
        self.tables_at = Some(self.tables_at.map_or(at, |was| was.max(at)));
        self
    }

    /// These observations with the server's allocation table read at `at` showing `row`,
    /// after the reads before.
    #[must_use]
    pub fn with_allocation_read(mut self, row: AllocationRow, at: FarmTime) -> Self {
        self.allocation_reads.push(AllocationRead { row, at });
        self.tables_at = Some(self.tables_at.map_or(at, |was| was.max(at)));
        self
    }

    /// These observations with `beat` sent, after the heartbeats before.
    #[must_use]
    pub fn with_job_beat(mut self, beat: JobBeat) -> Self {
        self.job_beats.push(beat);
        self
    }

    /// These observations with `reply` read, after the answers before.
    #[must_use]
    pub fn with_job_reply(mut self, reply: JobReply) -> Self {
        self.job_replies.push(reply);
        self
    }

    /// These observations with the agent of `host` having made the result of `job` at
    /// `at`, after the results before.
    #[must_use]
    pub fn with_result_made(mut self, host: HostId, job: JobId, at: FarmTime) -> Self {
        self.results_made.push(ResultMade { host, job, at });
        self
    }
}

/// The allocation id `text` names: 32 lowercase hex digits, as the server writes one.
pub(crate) fn allocation_id(text: &str) -> Option<AllocationId> {
    if text.len() != 32 {
        return None;
    }
    let mut bits = [0u8; 16];
    for (byte, pair) in bits.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(AllocationId::from_bits(bits))
}

/// The value of one lowercase hex digit.
fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: an allocation id read from text that is not the server's 32 lowercase hex
    /// digits (too short, too long, an odd length, a digit outside the alphabet), or read
    /// with its bytes or nibbles in another order.
    #[test]
    fn an_allocation_id_is_read_only_in_the_servers_form() {
        let bits = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let text = AllocationId::from_bits(bits).to_string();
        assert_eq!(allocation_id(&text), Some(AllocationId::from_bits(bits)));
        let odd = format!("{text}0");
        let high = format!("g{}", &text[1..]);
        let low = format!("0g{}", &text[2..]);
        let upper = text.to_uppercase();
        for bad in [
            "",
            &text[..30],
            odd.as_str(),
            high.as_str(),
            low.as_str(),
            upper.as_str(),
        ] {
            assert_eq!(allocation_id(bad), None, "{bad:?}");
        }
    }
}
