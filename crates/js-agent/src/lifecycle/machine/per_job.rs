//! Starts, stops and listings keyed by job id: `Command::JobStart` and `Command::JobStop`
//! in a reply, `jobs` and `retired_jobs` in a heartbeat, `job_acks` and `retired_acks` in
//! the reply that answers it (see `js_agent_wire::per_job`).
//!
//! A job start is made into the start a run start makes (the run id is the job id, the lease
//! id the allocation id, `valid_for_ms` is 0 so no lease bounds it, and the limits are the
//! booked resources with the job's wall limit) and goes through the one `start`: admission,
//! mounts, slots, refusals and the wall clock are the run's. Only the keying differs: the job
//! is listed in `jobs` with its result inside, acknowledged by `job_acks`, and never listed in
//! `held` or `results` nor touched by a `Stop`, `Cancel` or `result_acks` entry, so a lease
//! server that knows no job ids never sees one.

use std::collections::BTreeMap;

use js_agent_wire::{
    HeldJob, Isolation, JobResult, JobResultAck, JobSpec, JobStartSpec, Limits, RunResult, limits,
};

use super::Lifecycle;
use super::RETIRED_CAP;
use super::job::Keyed;
use crate::lifecycle::Event;

/// The most retired job ids a heartbeat lists in `retired_jobs`. Each is at most
/// [`limits::MAX_ID_LEN`] bytes, so the list weighs at most a quarter of the heartbeat body
/// whatever the ids: the stops of a server that sends many and acknowledges none cannot make
/// a heartbeat the server refuses. The tails of the results give way to it (`tails`).
pub const RETIRED_LISTED: usize = 96;
const _: () = assert!(
    RETIRED_LISTED * (limits::MAX_ID_LEN + 3) <= limits::HEARTBEAT_BODY_BUDGET / 4,
    "the retired job ids must fit a quarter of a heartbeat"
);
const _: () = assert!(RETIRED_LISTED < RETIRED_CAP);
const _: () = assert!(RETIRED_LISTED <= limits::MAX_HELD);

/// The start a job start stands for: one contained job with no mounts and no labels of its
/// own, bounded by what its allocation booked and by its own wall limit, with the network
/// and ports it asked for, which the shared admission holds to the runtime's ceiling.
fn run_of(spec: &JobStartSpec) -> JobSpec {
    JobSpec {
        run_id: spec.job_id.clone(),
        lease_id: spec.allocation_id.clone(),
        image: spec.image.clone(),
        run: spec.run.clone(),
        os: spec.os,
        arch: spec.arch,
        native: None,
        args: spec.command.clone(),
        limits: Limits {
            cpu_millis: spec.booked.cpu_millis,
            memory_bytes: spec.booked.memory_bytes,
            disk_bytes: spec.booked.disk_bytes,
            pids: None,
            wall_ms: spec.wall_ms,
        },
        gpu_indices: spec.booked.gpu_indices.clone(),
        mounts: Vec::new(),
        files: spec.files.clone(),
        isolation: Isolation::Contained,
        labels: BTreeMap::new(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
        bundle: None,
        network: spec.network,
        ports: spec.ports.clone(),
    }
}

/// The result of a job keyed by job id, as its listing carries it.
pub(super) fn job_result(result: &RunResult) -> JobResult {
    JobResult {
        job_id: result.run_id.clone(),
        allocation_id: result.lease_id.clone(),
        outcome: result.outcome,
        exit_code: result.exit_code,
        detail: result.detail.clone(),
        log_tail: result.log_tail.clone(),
    }
}

impl Lifecycle {
    /// Acts on a job start: the same start as a run's, keyed by job id. It is idempotent on
    /// the job id: a start for a job the agent holds changes nothing, and one whose fence
    /// (the job id with the allocation id) is retired is skipped (`Skip::FenceRetired`); the
    /// same job under a new allocation is new work. The job's environment goes to the
    /// runtime by name in order ([`RunSpec::env`](crate::lifecycle::RunSpec::env)); whether
    /// the runtime can carry it is the runtime's to say, never the lifecycle's.
    pub(super) fn start_job(
        &mut self,
        spec: &JobStartSpec,
        received: u64,
        events: &mut Vec<Event>,
    ) {
        let run = run_of(spec);
        if self.may_start((&run, Keyed::Job), received, events) {
            let env: Vec<(String, String)> = spec.env.clone().into_iter().collect();
            self.admit_and_start((&run, Keyed::Job), &env, received, events);
        }
    }

    /// Acts on a job stop. A held job is asked to stop as a `Stop` asks a run (the stop
    /// grace, then a kill) and ends `killed`; a second stop changes nothing. Its fence is
    /// retired at once, so the list in the state root names it before the stop is reported.
    /// A job the agent does not hold has every fence of its id retired (the stop names no
    /// allocation), so a later start of it runs nothing, and is listed in `retired_jobs`
    /// until the server acknowledges it.
    pub(super) fn stop_job(&mut self, job_id: &str) {
        let held = self
            .jobs
            .iter_mut()
            .find(|job| job.keyed == Keyed::Job && job.run_id == job_id);
        match held {
            Some(job) => {
                job.request_stop(false);
                self.retired.insert_with(&job.run_id, Some(&job.lease_id));
            }
            None => self.answer_stop(job_id),
        }
    }

    /// Retires `job_id` and lists it in `retired_jobs`, once. The list keeps the newest
    /// [`RETIRED_LISTED`] ids; an older one stays retired (the retired set is larger) and is
    /// listed again when the server resends its stop.
    fn answer_stop(&mut self, job_id: &str) {
        self.retired.insert(job_id);
        if self.answered.iter().any(|id| id == job_id) {
            return;
        }
        self.answered.push(job_id.to_owned());
        if self.answered.len() > RETIRED_LISTED {
            self.answered.remove(0);
        }
    }

    /// Drops the result of the job a `job_acks` entry names, on a verdict the agent knows.
    pub(super) fn acknowledge_job(&mut self, ack: &JobResultAck) {
        self.drop_result((Keyed::Job, &ack.job_id), ack.ack);
    }

    /// Stops listing the retired job ids the server recorded.
    pub(super) fn acknowledge_retired(&mut self, acked: &[String]) {
        self.answered.retain(|id| !acked.contains(id));
    }

    /// The jobs a job start began, for the `jobs` of the next heartbeat, in the order of
    /// [`Lifecycle::held`], each with its result once it has ended. A job is listed until
    /// its container and scratch are gone and its result is acknowledged.
    pub fn held_jobs(&self) -> Vec<HeldJob> {
        self.keyed(Keyed::Job)
            .map(|job| HeldJob {
                job_id: job.run_id.clone(),
                state: job.held_state(),
                result: job.result.as_ref().map(job_result),
            })
            .collect()
    }

    /// The job ids retired on a stop for a job the agent did not hold, for the
    /// `retired_jobs` of the next heartbeat, oldest first, until acknowledged.
    pub fn retired_jobs(&self) -> Vec<String> {
        self.answered.clone()
    }
}
