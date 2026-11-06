//! The five job invariants: [`Invariant::OneAllocationPerJob`],
//! [`Invariant::LostIsTerminal`], [`Invariant::FreedOnlyWhenConfirmed`],
//! [`Invariant::NoStartWithoutAllocation`] and [`Invariant::ResultKeptUntilAcknowledged`],
//! step checks over the server's job and allocation tables as the cell read them and over
//! what the per-job agents sent and read (`crate::jobs`).
//!
//! Each step reads only what was appended since the step before: the table reads, the
//! heartbeats, the answers and the results, in that order of kinds, each looked up in the
//! indices kept here; the agents the step touched are read for the jobs they run. A check
//! that compares two facts compares their virtual times, so the order in which one step
//! appends them does not matter.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationId, AllocationStep, EndReason, JobId, JobState};
use js_sim::text::time;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::{Found, Invariant, Touched};
use crate::jobs::read::{state_name, step_name};
use crate::jobs::{AllocationRow, JobReply, JobStartSeen};
use crate::observe::{Observations, Verdict};
use crate::text;

/// What the job checks have read so far.
#[derive(Clone, Debug, Default)]
pub(super) struct Indices {
    /// How many of each log were read.
    job_reads: usize,
    allocation_reads: usize,
    beats: usize,
    replies: usize,
    results: usize,
    /// When each job was first read lost.
    lost_at: BTreeMap<JobId, FarmTime>,
    /// The last row read of each allocation.
    rows: BTreeMap<AllocationId, AllocationRow>,
    /// The allocations each job was read with.
    by_job: BTreeMap<JobId, BTreeSet<AllocationId>>,
    /// The heartbeats of each host, by their place in the log.
    beats_of: BTreeMap<HostId, Vec<usize>>,
    /// Each start an agent read and no read of the tables since has judged: its host, when
    /// it was read, and what it named.
    pending: Vec<(HostId, FarmTime, JobStartSeen)>,
    /// The results each host made and no answer it read has acknowledged, with when.
    outstanding: BTreeMap<HostId, BTreeMap<JobId, FarmTime>>,
    /// When each host first read a verdict on each job's result.
    acked: BTreeMap<(HostId, JobId), FarmTime>,
    /// When each host first read a start of each job.
    started: BTreeMap<(HostId, JobId), FarmTime>,
    /// The first `Accepted` verdict on each job's result: the host that read it, and when.
    accepted: BTreeMap<JobId, (HostId, FarmTime)>,
    /// The hosts each job was seen running on and, when last asked, still ran it.
    runner: BTreeMap<JobId, BTreeSet<HostId>>,
}

impl Indices {
    /// The job checks of one step that touched `touched`.
    pub(super) fn step(
        &mut self,
        observations: &Observations,
        touched: &Touched,
        found: &mut Found,
    ) {
        self.index_beats(observations);
        self.read_jobs(observations, found);
        self.read_results(observations);
        self.read_replies(observations, found);
        self.read_allocations(observations, found);
        self.judge_starts(observations, found);
        self.check_beats(observations, found);
        for host in &touched.agents {
            self.read_agent(observations, host, found);
        }
    }

    /// Notes each heartbeat appended, by host.
    fn index_beats(&mut self, observations: &Observations) {
        let new = observations.job_beats.iter().enumerate().skip(self.beats);
        for (at, beat) in new {
            self.beats_of.entry(beat.host.clone()).or_default().push(at);
        }
    }

    /// `LostIsTerminal` (a): no job read lost is read in another state after.
    fn read_jobs(&mut self, observations: &Observations, found: &mut Found) {
        for read in observations.job_reads.iter().skip(self.job_reads) {
            let lost = self.lost_at.get(&read.job).copied();
            match (lost, read.state) {
                (None, JobState::Lost) => {
                    self.lost_at.insert(read.job.clone(), read.at);
                }
                (Some(lost), state) if state != JobState::Lost => {
                    let detail = format!(
                        "job {} read {} at {} after it read lost at {}",
                        read.job,
                        state_name(state),
                        time(read.at),
                        time(lost)
                    );
                    found.flag(Invariant::LostIsTerminal, detail);
                }
                _ => {}
            }
        }
        self.job_reads = observations.job_reads.len();
    }

    /// `OneAllocationPerJob` (a) and (b), and `FreedOnlyWhenConfirmed`, over the
    /// allocation rows read since the last step.
    fn read_allocations(&mut self, observations: &Observations, found: &mut Found) {
        let mut jobs = BTreeSet::new();
        for read in observations
            .allocation_reads
            .iter()
            .skip(self.allocation_reads)
        {
            let row = &read.row;
            let before = self.rows.insert(row.allocation, row.clone());
            if let Some(before) = &before {
                reused(before, row, read.at, found);
            }
            let was_freed = before.is_some_and(|b| b.step == AllocationStep::Freed);
            if (row.step == AllocationStep::Freed) & !was_freed {
                self.freed(observations, row, read.at, found);
            }
            let allocations = self.by_job.entry(row.job.clone()).or_default();
            allocations.insert(row.allocation);
            jobs.insert(row.job.clone());
        }
        self.allocation_reads = observations.allocation_reads.len();
        for job in jobs {
            let ids = self.by_job.get(&job).into_iter().flatten();
            let booked = ids.filter(|id| {
                let row = self.rows.get(*id);
                row.is_some_and(|row| row.step == AllocationStep::Booked)
            });
            let booked: Vec<String> = booked.map(ToString::to_string).collect();
            if let [first, second, ..] = booked.as_slice() {
                let detail =
                    format!("job {job} has allocations {first} and {second} booked at once");
                found.flag(Invariant::OneAllocationPerJob, detail);
            }
        }
    }

    /// Notes each result made since the last step as outstanding until acknowledged.
    fn read_results(&mut self, observations: &Observations) {
        for made in observations.results_made.iter().skip(self.results) {
            let host = self.outstanding.entry(made.host.clone()).or_default();
            host.insert(made.job.clone(), made.at);
        }
        self.results = observations.results_made.len();
    }

    /// Over the answers read since the last step: their verdicts
    /// (`ResultKeptUntilAcknowledged` (b), and the acknowledgements (a) reads), their starts
    /// (kept for `NoStartWithoutAllocation`), and their stops (`LostIsTerminal` (b)).
    fn read_replies(&mut self, observations: &Observations, found: &mut Found) {
        for reply in observations.job_replies.iter().skip(self.replies) {
            for (job, verdict) in &reply.acks {
                let key = (reply.host.clone(), job.clone());
                self.acked.entry(key).or_insert(reply.at);
                if *verdict == Verdict::Accepted {
                    self.accept(job, reply, found);
                }
            }
            for start in &reply.starts {
                let key = (reply.host.clone(), start.job.clone());
                self.started.entry(key).or_insert(reply.at);
            }
            let starts = reply.starts.iter().cloned();
            self.pending
                .extend(starts.map(|start| (reply.host.clone(), reply.at, start)));
            self.stops_for_lost(reply, found);
        }
        self.replies = observations.job_replies.len();
    }

    /// `ResultKeptUntilAcknowledged` (b): no job's result is accepted twice.
    fn accept(&mut self, job: &JobId, reply: &JobReply, found: &mut Found) {
        let Some((host, at)) = self.accepted.get(job) else {
            let first = (reply.host.clone(), reply.at);
            self.accepted.insert(job.clone(), first);
            return;
        };
        let detail = format!(
            "the result of job {job} was accepted twice: on host {} at {} and on host {} at {}",
            text::host(host),
            time(*at),
            text::host(&reply.host),
            time(reply.at)
        );
        found.flag(Invariant::ResultKeptUntilAcknowledged, detail);
    }

    /// `LostIsTerminal` (b): an answer to a heartbeat that listed a job running after the
    /// job read lost carries a stop of it.
    fn stops_for_lost(&self, reply: &JobReply, found: &mut Found) {
        for job in &reply.listed {
            let lost = self.lost_at.get(job).filter(|lost| **lost <= reply.beat_at);
            let Some(lost) = lost.filter(|_| !reply.stops.contains(job)) else {
                continue;
            };
            let detail = format!(
                "host {} listed job {job} at {}, after it read lost at {}, and the answer carried no stop of it",
                text::host(&reply.host),
                time(reply.beat_at),
                time(*lost)
            );
            found.flag(Invariant::LostIsTerminal, detail);
        }
    }

    /// `NoStartWithoutAllocation`: each start read before the last read of the tables
    /// names an allocation of that job on that host that the server committed no later.
    fn judge_starts(&mut self, observations: &Observations, found: &mut Found) {
        let Some(tables_at) = observations.tables_at else {
            return;
        };
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(_, at, _)| *at < tables_at);
        self.pending = later;
        for (host, at, start) in due {
            let row = self.rows.get(&start.allocation);
            let committed = row.filter(|row| (row.job == start.job) & (row.host == host));
            if committed.is_some_and(|row| row.created_at <= at) {
                continue;
            }
            let detail = format!(
                "host {} was sent a start of job {} under allocation {} at {} that no allocation committed before it names",
                text::host(&host),
                start.job,
                start.allocation,
                time(at)
            );
            found.flag(Invariant::NoStartWithoutAllocation, detail);
        }
    }

    /// `ResultKeptUntilAcknowledged` (a): each heartbeat sent since the last step lists
    /// every result its host made before it and had not read a verdict on, unless the
    /// host's process was killed in between.
    fn check_beats(&mut self, observations: &Observations, found: &mut Found) {
        for beat in observations.job_beats.iter().skip(self.beats) {
            let Some(outstanding) = self.outstanding.get(&beat.host) else {
                continue;
            };
            for (job, made) in outstanding {
                let acked = self.acked.get(&(beat.host.clone(), job.clone()));
                let kept = beat.results.contains(job) | (*made >= beat.at);
                let excused = acked.is_some_and(|acked| *acked <= beat.at)
                    | killed_between(observations, &beat.host, *made, beat.at);
                if kept | excused {
                    continue;
                }
                let detail = format!(
                    "host {} sent a heartbeat at {} without the result of job {job} it made at {}, before any answer acknowledged it",
                    text::host(&beat.host),
                    time(beat.at),
                    time(*made)
                );
                found.flag(Invariant::ResultKeptUntilAcknowledged, detail);
            }
        }
        self.beats = observations.job_beats.len();
        let acked = &self.acked;
        for (host, jobs) in &mut self.outstanding {
            jobs.retain(|job, _| !acked.contains_key(&(host.clone(), job.clone())));
        }
    }

    /// `OneAllocationPerJob` (c) for one touched agent: no job runs on two hosts at once.
    fn read_agent(&mut self, observations: &Observations, host: &HostId, found: &mut Found) {
        let Some(agent) = observations.agents.get(host) else {
            return;
        };
        for job in agent.jobs.keys() {
            let owners = self.runner.entry(job.clone()).or_default();
            owners.retain(|other| (other == host) | runs_job(observations, other, job));
            if let Some(other) = owners.iter().find(|other| *other != host) {
                let detail = format!(
                    "job {job} runs on host {} and on host {}",
                    text::host(other),
                    text::host(host)
                );
                found.flag(Invariant::OneAllocationPerJob, detail);
            }
            owners.insert(host.clone());
        }
    }
}

/// `OneAllocationPerJob` (b): a row read again for an allocation names the job and host it
/// named before, and an allocation read freed is never read live again.
fn reused(before: &AllocationRow, row: &AllocationRow, at: FarmTime, found: &mut Found) {
    let moved = (before.job != row.job) | (before.host != row.host);
    let revived = (before.step == AllocationStep::Freed) & (row.step != AllocationStep::Freed);
    let detail = if moved {
        format!(
            "allocation {} of job {} on host {} was read at {} for job {} on host {}",
            row.allocation,
            before.job,
            text::host(&before.host),
            time(at),
            row.job,
            text::host(&row.host)
        )
    } else if revived {
        format!(
            "allocation {} of job {} was read {} at {} after it was read freed",
            row.allocation,
            row.job,
            step_name(row.step),
            time(at)
        )
    } else {
        return;
    };
    found.flag(Invariant::OneAllocationPerJob, detail);
}

impl Indices {
    /// `FreedOnlyWhenConfirmed` for one allocation read freed at `at`: the host's agent
    /// sent, by `at`, a heartbeat that did not list the job (or listed it retired), after
    /// the allocation was committed, after the agent read the job's start (when it read
    /// one) and after it read a verdict on the job's result (when it read one); or the
    /// server's listing read with the tables does not name the host (the provider reads it
    /// absent); or a whole-host prepare failed before any container of the placement was
    /// launched. A heartbeat sent before the agent knew of the job, or while it still held
    /// its result, is not the host's word that the container is gone.
    fn freed(
        &self,
        observations: &Observations,
        row: &AllocationRow,
        at: FarmTime,
        found: &mut Found,
    ) {
        let key = (row.host.clone(), row.job.clone());
        let started = self.started.get(&key).copied();
        let acked = self.acked.get(&key).copied();
        let beats = self.beats_of.get(&row.host).into_iter().flatten();
        let beats = beats.filter_map(|index| observations.job_beats.get(*index));
        let mut since = beats.filter(|beat| {
            let after = (beat.at >= row.created_at)
                & started.is_none_or(|started| beat.at > started)
                & acked.is_none_or(|acked| beat.at > acked);
            after & (beat.at <= at)
        });
        let gone = since.any(|beat| {
            let listed = beat.running.contains(&row.job) | beat.results.contains(&row.job);
            !listed
        });
        let absent = !observations.server.hosts.contains_key(&row.host);
        let launched = format!("{}/{}", row.job, row.allocation);
        let never_ran = !observations
            .ledger
            .iter()
            .any(|l| l.run.as_str() == launched);
        let failed_prepare = (row.end_reason == Some(EndReason::FailedPrepare)) & never_ran;
        if gone | absent | failed_prepare {
            return;
        }
        let detail = format!(
            "allocation {} of job {} on host {} was read freed at {} with no listing of the host since {} that left the job out, while the server lists the host",
            row.allocation,
            row.job,
            text::host(&row.host),
            time(at),
            time(row.created_at)
        );
        found.flag(Invariant::FreedOnlyWhenConfirmed, detail);
    }
}

/// Whether the cell killed the process of `host` at or after `from` and by `to`.
fn killed_between(
    observations: &Observations,
    host: &HostId,
    from: FarmTime,
    to: FarmTime,
) -> bool {
    let mut kills = observations.kills.iter();
    kills.any(|kill| (&kill.host == host) & (kill.at >= from) & (kill.at <= to))
}

/// Whether the agent of `host` runs `job` now.
fn runs_job(observations: &Observations, host: &HostId, job: &JobId) -> bool {
    let agent = observations.agents.get(host);
    agent.is_some_and(|agent| agent.jobs.contains_key(job))
}
