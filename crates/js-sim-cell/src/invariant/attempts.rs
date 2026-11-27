//! The three job invariants of retry and the completion callback:
//! [`Invariant::AttemptsBounded`], [`Invariant::OneAttemptAtATime`] and
//! [`Invariant::CallbackAfterTheFinalEnd`], step checks over the launch ledger, the job
//! table's attempt reads, the agents' truth, the starts the agents read and the calls the
//! cell's callback receiver got.
//!
//! Each step reads only what was appended since the step before, and the agents the step
//! touched.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationId, JobId};
use js_sim::text::time;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::{Found, Invariant, Touched};
use crate::jobs::read::state_name;
use crate::observe::Observations;
use crate::text;

/// What the attempt checks have read so far.
#[derive(Clone, Debug, Default)]
pub(super) struct Indices {
    /// How many of each log were read.
    ledger: usize,
    attempts: usize,
    replies: usize,
    calls: usize,
    /// The allocations each job was launched under, and when it was last launched.
    launches: BTreeMap<JobId, (BTreeSet<String>, FarmTime)>,
    /// The most attempts each job asked for, as the last read of its row showed.
    limits: BTreeMap<JobId, u32>,
    /// How many launches of each job were flagged, so a launch is named once.
    flagged: BTreeMap<JobId, usize>,
    /// The allocation each host's agent holds each job under, by the agents' truth.
    holding: BTreeMap<JobId, BTreeMap<HostId, AllocationId>>,
    /// The jobs each touched host's agent held when it was last read.
    held_by: BTreeMap<HostId, BTreeSet<JobId>>,
    /// The first webhook id each job's calls carried, and when.
    webhook: BTreeMap<JobId, (String, FarmTime)>,
}

impl Indices {
    /// The attempt checks of one step that touched `touched`.
    pub(super) fn step(
        &mut self,
        observations: &Observations,
        touched: &Touched,
        found: &mut Found,
    ) {
        self.bounded(observations, found);
        for host in &touched.agents {
            self.read_agent(observations, host);
        }
        self.one_at_a_time(observations, found);
        self.calls(observations, found);
    }

    /// `AttemptsBounded`: over the launches and attempt reads appended since the last step,
    /// no job is launched under more allocations than its policy allows, and no attempt
    /// number read exceeds it. A job with no read of its row has no known limit.
    fn bounded(&mut self, observations: &Observations, found: &mut Found) {
        let mut changed = BTreeSet::new();
        for launch in observations.ledger.iter().skip(self.ledger) {
            let Some((job, allocation)) = launch.run.as_str().split_once('/') else {
                continue;
            };
            let Ok(job) = JobId::parse(job) else {
                continue;
            };
            let entry = (BTreeSet::new(), launch.at);
            let (allocations, last) = self.launches.entry(job.clone()).or_insert(entry);
            if allocations.insert(allocation.to_owned()) {
                *last = launch.at;
            }
            changed.insert(job);
        }
        self.ledger = observations.ledger.len();
        for read in observations.attempt_reads.iter().skip(self.attempts) {
            self.limits.insert(read.job.clone(), read.max_attempts);
            changed.insert(read.job.clone());
            if read.number > read.max_attempts {
                let detail = format!(
                    "job {} read attempt {} at {}, its policy allows {}",
                    read.job,
                    read.number,
                    time(read.at),
                    read.max_attempts
                );
                found.flag(Invariant::AttemptsBounded, detail);
            }
        }
        self.attempts = observations.attempt_reads.len();
        for job in changed {
            self.launched_too_often(&job, found);
        }
    }

    /// `AttemptsBounded` for the launches of one job, flagged once for each launch past the
    /// limit.
    fn launched_too_often(&mut self, job: &JobId, found: &mut Found) {
        let (Some(limit), Some((allocations, last))) =
            (self.limits.get(job), self.launches.get(job))
        else {
            return;
        };
        let count = allocations.len();
        let limit = *limit;
        let past = usize::try_from(limit).is_ok_and(|limit| count > limit);
        if !past || self.flagged.get(job).is_some_and(|done| *done >= count) {
            return;
        }
        let detail = format!(
            "job {job} was launched under {count} allocations, the last at {}, its policy allows {limit}",
            time(*last)
        );
        self.flagged.insert(job.clone(), count);
        found.flag(Invariant::AttemptsBounded, detail);
    }

    /// Notes the jobs the agent of `host` holds, and under which allocation.
    fn read_agent(&mut self, observations: &Observations, host: &HostId) {
        for job in self.held_by.remove(host).into_iter().flatten() {
            self.holding.entry(job).or_default().remove(host);
        }
        let Some(agent) = observations.agents.get(host) else {
            return;
        };
        for (job, running) in &agent.jobs {
            let hosts = self.holding.entry(job.clone()).or_default();
            hosts.insert(host.clone(), running.allocation);
            self.held_by
                .entry(host.clone())
                .or_default()
                .insert(job.clone());
        }
    }

    /// `OneAttemptAtATime`: no agent reads the start of a job under an allocation while an
    /// agent (the same or another) holds the job under another one, by the agents' truth.
    fn one_at_a_time(&mut self, observations: &Observations, found: &mut Found) {
        for reply in observations.job_replies.iter().skip(self.replies) {
            for start in reply.starts.iter().filter(|start| !start.refused) {
                let holders = self.holding.get(&start.job).into_iter().flatten();
                let mut others = holders.filter(|(_, held)| **held != start.allocation);
                let Some((other, held)) = others.next() else {
                    continue;
                };
                let detail = format!(
                    "host {} read a start of job {} under allocation {} at {} while host {} still holds the job under allocation {}",
                    text::host(&reply.host),
                    start.job,
                    start.allocation,
                    time(reply.at),
                    text::host(other),
                    held
                );
                found.flag(Invariant::OneAttemptAtATime, detail);
            }
        }
        self.replies = observations.job_replies.len();
    }

    /// `CallbackAfterTheFinalEnd`: each call the receiver got since the last step was made
    /// after the job's row ended, and carries the webhook id of the job's first call.
    fn calls(&mut self, observations: &Observations, found: &mut Found) {
        for call in observations.callback_calls.iter().skip(self.calls) {
            if !call.ended {
                let row = call.state.map_or("absent", state_name);
                let detail = format!(
                    "the callback of job {} was called at {} while its row read {row}, not ended",
                    call.job,
                    time(call.at)
                );
                found.flag(Invariant::CallbackAfterTheFinalEnd, detail);
            }
            let first = self
                .webhook
                .entry(call.job.clone())
                .or_insert_with(|| (call.webhook_id.clone(), call.at));
            if first.0 != call.webhook_id {
                let detail = format!(
                    "the callback of job {} was called at {} with webhook id {}, after webhook id {} at {}",
                    call.job,
                    time(call.at),
                    call.webhook_id,
                    first.0,
                    time(first.1)
                );
                found.flag(Invariant::CallbackAfterTheFinalEnd, detail);
            }
        }
        self.calls = observations.callback_calls.len();
    }
}
