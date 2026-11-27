//! The cell's launch ledger: one entry per job an agent process launched and, for the
//! whole-host handshake, one per ready report an agent sent.
//!
//! The ledger belongs to the cell, not to an agent. Every agent node writes into one
//! shared [`Ledger`] handle that outlives the node's processes, so an agent that is
//! killed and replaced cannot reset a count. [`Ledger::launches`] is what the cell puts
//! in [`crate::Observations::ledger`] for [`crate::Invariant::AtMostOneLaunch`]: a
//! replacement that launches a job its predecessor launched shows as a second entry.
//!
//! A per-job agent also writes each heartbeat it sends, each answer it reads and each
//! result it makes ([`JobBeat`], [`JobReply`], [`ResultMade`]), for the job invariants;
//! its launches go into the launches, each named `<job>/<allocation>`.

use std::cell::RefCell;
use std::rc::Rc;

use js_types::HostId;
use js_types::sched::FarmTime;

use crate::jobs::{JobBeat, JobReply, ResultMade};
use crate::observe::{Launch, ReadyReport, RunId, Verdict};

/// One run that ended on its own (its job's time ran out), as the observations record it:
/// no agent writes one now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The run.
    pub run: RunId,
    /// The host whose agent ran it.
    pub host: HostId,
    /// When it ended.
    pub at: FarmTime,
}

/// One verdict an agent process read on the result of one run, as the observations record
/// it: no agent writes one now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acked {
    /// The run whose result was judged.
    pub run: RunId,
    /// The host whose agent read the verdict.
    pub host: HostId,
    /// The verdict.
    pub verdict: Verdict,
    /// When the agent read it.
    pub at: FarmTime,
}

/// The cell's ledger, shared: every clone is a handle on the same entries.
///
/// The cell is single-threaded (the kernel applies one input at a time), so the handle
/// is an `Rc`, and the ledger is only ever appended to.
#[derive(Clone, Debug, Default)]
pub struct Ledger(Rc<RefCell<Entries>>);

#[derive(Debug, Default)]
struct Entries {
    launches: Vec<Launch>,
    ready: Vec<ReadyReport>,
    job_beats: Vec<JobBeat>,
    job_replies: Vec<JobReply>,
    results_made: Vec<ResultMade>,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every launch, oldest first.
    #[must_use]
    pub fn launches(&self) -> Vec<Launch> {
        self.0.borrow().launches.clone()
    }

    /// The launches after the first `skip`, oldest first.
    pub(crate) fn launches_after(&self, skip: usize) -> Vec<Launch> {
        self.0
            .borrow()
            .launches
            .iter()
            .skip(skip)
            .cloned()
            .collect()
    }

    /// Appends a launch.
    pub(crate) fn launched(&self, launch: Launch) {
        self.0.borrow_mut().launches.push(launch);
    }

    /// Every ready report sent, oldest first.
    #[must_use]
    pub fn ready(&self) -> Vec<ReadyReport> {
        self.0.borrow().ready.clone()
    }

    /// The ready reports after the first `skip`, oldest first.
    pub(crate) fn ready_after(&self, skip: usize) -> Vec<ReadyReport> {
        self.0.borrow().ready.iter().skip(skip).cloned().collect()
    }

    /// Appends a ready report.
    pub(crate) fn reported_ready(&self, report: ReadyReport) {
        self.0.borrow_mut().ready.push(report);
    }

    /// The per-job heartbeats, answers and results after the first `skip` of each, oldest
    /// first.
    pub(crate) fn jobs_after(
        &self,
        [beats, replies, results]: [usize; 3],
    ) -> (Vec<JobBeat>, Vec<JobReply>, Vec<ResultMade>) {
        let entries = self.0.borrow();
        (
            entries.job_beats.iter().skip(beats).cloned().collect(),
            entries.job_replies.iter().skip(replies).cloned().collect(),
            entries.results_made.iter().skip(results).cloned().collect(),
        )
    }

    /// Appends a per-job heartbeat sent.
    pub(crate) fn job_beat(&self, beat: JobBeat) {
        self.0.borrow_mut().job_beats.push(beat);
    }

    /// Appends an answer a per-job agent read.
    pub(crate) fn job_reply(&self, reply: JobReply) {
        self.0.borrow_mut().job_replies.push(reply);
    }

    /// Appends a result a per-job agent made.
    pub(crate) fn result_made(&self, made: ResultMade) {
        self.0.borrow_mut().results_made.push(made);
    }
}
