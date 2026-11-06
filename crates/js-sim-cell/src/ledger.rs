//! The cell's launch and completion ledger: one entry per run an agent process launched,
//! one per run that ended on its own, one per verdict an agent process read on one of its
//! results, and, for the whole-host handshake, one per ready report an agent sent, per
//! start delivered and per stop or cancel that reached a running run.
//!
//! The ledger belongs to the cell, not to an agent. Every agent node writes into one
//! shared [`Ledger`] handle that outlives the node's processes, so an agent that is
//! killed and replaced cannot reset a count. [`Ledger::launches`] is what the cell puts
//! in [`crate::Observations::ledger`] for [`crate::Invariant::AtMostOneLaunch`]: a
//! replacement that launches a run its predecessor launched shows as a second entry. The
//! completions and the verdicts are what [`crate::Invariant::NoResultLost`] reads: a
//! process that dies takes its own record of them with it, the ledger keeps them.
//!
//! A per-job agent also writes each heartbeat it sends, each answer it reads and each
//! result it makes ([`JobBeat`], [`JobReply`], [`ResultMade`]), for the job invariants;
//! its launches go into the launches beside the runs', each named `<job>/<allocation>`.

use std::cell::RefCell;
use std::rc::Rc;

use js_types::HostId;
use js_types::sched::FarmTime;

use crate::jobs::{JobBeat, JobReply, ResultMade};
use crate::observe::{Launch, ReadyReport, RunId, StartSeen, StopSeen, Verdict};

/// One run that ended on its own (its job's time ran out), in the cell's ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The run.
    pub run: RunId,
    /// The host whose agent ran it.
    pub host: HostId,
    /// When it ended.
    pub at: FarmTime,
}

/// One verdict an agent process read in an answer to one of its heartbeats, on the
/// result of one run, in the cell's ledger. Every verdict read is written, whether or not
/// the process still held the result.
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
    completions: Vec<Completion>,
    verdicts: Vec<Acked>,
    ready: Vec<ReadyReport>,
    starts: Vec<StartSeen>,
    stops: Vec<StopSeen>,
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

    /// Every completion, oldest first.
    #[must_use]
    pub fn completions(&self) -> Vec<Completion> {
        self.0.borrow().completions.clone()
    }

    /// The completions after the first `skip`, oldest first.
    pub(crate) fn completions_after(&self, skip: usize) -> Vec<Completion> {
        let entries = self.0.borrow();
        entries.completions.iter().skip(skip).cloned().collect()
    }

    /// Every verdict an agent read, oldest first.
    #[must_use]
    pub fn verdicts(&self) -> Vec<Acked> {
        self.0.borrow().verdicts.clone()
    }

    /// The verdicts after the first `skip`, oldest first.
    pub(crate) fn verdicts_after(&self, skip: usize) -> Vec<Acked> {
        let entries = self.0.borrow();
        entries.verdicts.iter().skip(skip).cloned().collect()
    }

    /// Appends a launch.
    pub(crate) fn launched(&self, launch: Launch) {
        self.0.borrow_mut().launches.push(launch);
    }

    /// Appends a completion.
    pub(crate) fn completed(&self, completion: Completion) {
        self.0.borrow_mut().completions.push(completion);
    }

    /// Every ready report sent, oldest first.
    #[must_use]
    pub fn ready(&self) -> Vec<ReadyReport> {
        self.0.borrow().ready.clone()
    }

    /// Every start delivered, oldest first.
    #[must_use]
    pub fn starts(&self) -> Vec<StartSeen> {
        self.0.borrow().starts.clone()
    }

    /// Every stop or cancel that reached a running run, oldest first.
    #[must_use]
    pub fn stops(&self) -> Vec<StopSeen> {
        self.0.borrow().stops.clone()
    }

    /// The ready reports, starts and stops after the first `skip` of each, oldest first.
    pub(crate) fn whole_after(
        &self,
        [ready, starts, stops]: [usize; 3],
    ) -> (Vec<ReadyReport>, Vec<StartSeen>, Vec<StopSeen>) {
        let entries = self.0.borrow();
        (
            entries.ready.iter().skip(ready).cloned().collect(),
            entries.starts.iter().skip(starts).cloned().collect(),
            entries.stops.iter().skip(stops).cloned().collect(),
        )
    }

    /// Appends a ready report.
    pub(crate) fn reported_ready(&self, report: ReadyReport) {
        self.0.borrow_mut().ready.push(report);
    }

    /// Appends a delivered start.
    pub(crate) fn start_seen(&self, start: StartSeen) {
        self.0.borrow_mut().starts.push(start);
    }

    /// Appends a stop or cancel that reached a running run.
    pub(crate) fn stop_seen(&self, stop: StopSeen) {
        self.0.borrow_mut().stops.push(stop);
    }

    /// Appends a verdict.
    pub(crate) fn acked(&self, acked: Acked) {
        self.0.borrow_mut().verdicts.push(acked);
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
