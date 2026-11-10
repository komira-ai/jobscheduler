//! The whole-host invariants over jobs: [`Invariant::HeldMeansReady`] and
//! [`Invariant::DrainKillsNothing`] as the job front shows them. The job front hears no
//! lease, so the checks of `whole.rs` that read a lease's grant and a run's stop see
//! nothing of a job; these read what a job leaves instead: the server's job and
//! allocation tables as the cell read them, the ready reports the agents sent, and the
//! stops the server sent the per-job agents (`crate::jobs`).
//!
//! What is a whole host comes from what the client asked for ([`crate::Ask::whole_host`]
//! of the job's item, `sim-job-<n>` being item `n`), never from the server's tables. Each
//! step reads only what was appended since the step before.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationId, JobId, JobState};
use js_sim::text::time;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::jobs::JOB_PREFIX;
use crate::jobs::read::state_name;
use crate::observe::{ItemId, Observations};
use crate::text;

/// What the whole-host checks over jobs have read so far.
#[derive(Clone, Debug, Default)]
pub(super) struct Indices {
    /// How many of each log were read.
    job_reads: usize,
    allocation_reads: usize,
    replies: usize,
    /// The allocation last read of each job, with the host it is on.
    latest: BTreeMap<JobId, (AllocationId, HostId)>,
    /// The placements already judged: a job read starting or running for an allocation is
    /// judged once, at the first read that shows it.
    judged: BTreeSet<(JobId, AllocationId)>,
}

/// The item whose job the job front submits as `job`.
fn item_of(job: &JobId) -> Option<ItemId> {
    let number = job.as_str().strip_prefix(JOB_PREFIX)?;
    number.parse().ok().map(ItemId)
}

impl Indices {
    /// The checks of one step. `ready` is when each host first reported ready for each
    /// allocation (as the report wrote it).
    pub(super) fn step(
        &mut self,
        ready: &BTreeMap<(HostId, String), FarmTime>,
        observations: &Observations,
        found: &mut Found,
    ) {
        self.read_starts(ready, observations, found);
        self.read_stops(observations, found);
    }

    /// `HeldMeansReady` over the job reads appended since the last step: a whole-host job
    /// first read starting or running on an allocation does so only after the agent of the
    /// allocation's host sent a ready report naming exactly that allocation. The allocation
    /// is the one the tables showed at or before the read; a read's time is when the cell
    /// looked, so a report sent by then is one the server may have taken.
    fn read_starts(
        &mut self,
        ready: &BTreeMap<(HostId, String), FarmTime>,
        observations: &Observations,
        found: &mut Found,
    ) {
        let reads = &observations.job_reads;
        for read in reads.iter().skip(self.job_reads) {
            let allocations = &observations.allocation_reads;
            while let Some(shown) = allocations.get(self.allocation_reads) {
                if shown.at > read.at {
                    break;
                }
                let row = &shown.row;
                self.latest
                    .insert(row.job.clone(), (row.allocation, row.host.clone()));
                self.allocation_reads += 1;
            }
            let started = matches!(read.state, JobState::Starting | JobState::Running);
            let item = item_of(&read.job).and_then(|item| observations.items.get(&item));
            let whole = item.is_some_and(|record| record.ask.whole_host);
            let Some((allocation, host)) = self.latest.get(&read.job).filter(|_| started & whole)
            else {
                continue;
            };
            if !self.judged.insert((read.job.clone(), *allocation)) {
                continue;
            }
            let reported = ready.get(&(host.clone(), allocation.to_string()));
            if reported.is_none_or(|at| *at > read.at) {
                let detail = format!(
                    "whole-host job {} was read {} on allocation {} on host {} at {} before \
                     the host's agent reported ready for that allocation",
                    read.job,
                    state_name(read.state),
                    allocation,
                    text::host(host),
                    time(read.at)
                );
                found.flag(Invariant::HeldMeansReady, detail);
            }
        }
        self.job_reads = reads.len();
    }

    /// `DrainKillsNothing` over the answers appended since the last step: a stop of a job
    /// sent to a host the listing sets aside is one the job's client asked for (it asked to
    /// cancel the item by the time the answer was read), or a violation.
    fn read_stops(&mut self, observations: &Observations, found: &mut Found) {
        let replies = &observations.job_replies;
        for reply in replies.iter().skip(self.replies) {
            let listed = observations.server.hosts.get(&reply.host);
            if !listed.is_some_and(|listed| listed.set_aside) {
                continue;
            }
            for job in &reply.stops {
                let item = item_of(job);
                let asked = item.and_then(|item| observations.cancels.get(&item));
                if asked.is_some_and(|at| *at <= reply.at) {
                    continue;
                }
                let detail = format!(
                    "host {}, which the server sets aside, was sent a stop of job {job} at {} \
                     that its client did not ask for",
                    text::host(&reply.host),
                    time(reply.at)
                );
                found.flag(Invariant::DrainKillsNothing, detail);
            }
        }
        self.replies = replies.len();
    }
}
