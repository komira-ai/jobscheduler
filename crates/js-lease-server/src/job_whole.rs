//! The prepare of a whole-host job on its host (design sections 5.3 and 5.4, and section 8,
//! rows 15, 26 and 39): what a heartbeat of the host and the tick do to a job that is placed
//! with an allocation that books the whole host, while the agent prepares the host for it.
//!
//! The agent goes `preparing` when a reply names the allocation (`exclusive`), starts nothing
//! for another job, and reports `exclusive_ready` when the host is quiet or `exclusive_refused`
//! with a code. No container exists until the job is starting, so everything here is a change
//! of the job and allocation rows; the start rides on the reply once the job is starting
//! ([`crate::per_job`]).
//!
//! [`serve`] runs once per heartbeat of a host that speaks the per-job messages, before the
//! listing is committed. For each placed whole-host job on the host, the first rule that fits:
//!
//! 1. **A cancel intent.** The job is cancelled and the allocation freed, in one commit, as
//!    soon as the agent has left `preparing`: its phase is not `preparing`, or it reports the
//!    allocation ready or refused. Until then nothing is written, no stop is sent (the agent
//!    holds no job) and the allocation is not named, so the agent goes on to restore itself.
//! 2. **The first listing after a hello** restarts the prepare: the allocation's prepare
//!    deadline becomes the store's now plus `prepare_timeout`. A hello cannot change a job
//!    that is already starting or running; the host's listing decides it
//!    ([`crate::job_loss::lose_unlisted`]).
//! 3. **A ready report** naming the allocation, from an untainted host, moves the job to
//!    starting.
//! 4. **A refusal** naming the allocation fails the prepare ([`fail_prepare`]) with the
//!    agent's code.
//! 5. Otherwise nothing changes and the allocation stays named.
//!
//! [`expire_prepares`] runs in the tick, before the start timeouts ([`crate::job_pass`]): a
//! placed whole-host job whose allocation's prepare deadline has passed fails its prepare, or,
//! with a cancel intent, is cancelled and freed. The deadline is checked only there, never at a
//! heartbeat, so a ready report that reaches the server before the tick wins, and one that
//! arrives after it names an allocation that is no longer live.
//!
//! A failed prepare is the one free that needs no confirmation from the host, because no
//! container was made ([`js_jobs::JobStore::fail_prepare_with`]). The job goes back to queued
//! with its place in the queue, one more failed prepare counted and the host barred for it as
//! the cause says ([`js_sched_placer::engine::whole_jobs::failed_prepare`], which applies the
//! rules of a whole-node lease); after `max_replace` of them it fails to start with
//! `exclusive-not-ready`. A cause that taints the host (`host-unsafe`) also taints it in the
//! registry. Nothing here reads a clock: `now` is given.

use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, Phase, RefusalReason};
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, JobId, JobRecord, JobState, JobStoreError,
    StartFailure,
};
use js_registry::WholeNodeConfig;
use js_state::StoreTime;
use js_types::HostId;

use crate::job_alloc::{cancel_and_free, history};
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;
use crate::sched::engine::whole_jobs::{PrepareCause, failed_prepare};

/// What one heartbeat says to the whole-host job placed on its host.
pub(crate) struct Beat<'a> {
    /// The host whose heartbeat this is.
    pub(crate) host: &'a HostId,
    /// The phase the agent reports.
    pub(crate) phase: Phase,
    /// The allocation id the agent reports ready, if it does.
    pub(crate) ready: Option<&'a str>,
    /// The refusal the agent reports, if it does.
    pub(crate) refused: Option<&'a ExclusiveRefusal>,
    /// Whether this is the first job listing since the host's hello.
    pub(crate) first: bool,
    /// Whether the registry holds the host tainted.
    pub(crate) tainted: bool,
}

/// What a heartbeat does to one placed whole-host job.
enum Step {
    /// The agent has left `preparing` and the job has a cancel intent: cancel it and free the
    /// allocation.
    Release,
    /// A hello: the prepare starts again.
    Restart,
    /// The agent reported the host ready: the job is starting.
    Start,
    /// The agent refused the host, with this code: the prepare failed.
    Fail(RefusalReason),
    /// Nothing changes.
    Wait,
}

/// The job of `allocation`, when `allocation` books the whole host, is booked and `job` is
/// placed: the one a prepare is for. `None` for anything else.
fn placed_whole(allocation: &AllocationRecord, job: Option<JobRecord>) -> Option<JobRecord> {
    let booked = matches!(allocation.booked, Booked::WholeHost)
        & (allocation.step == AllocationStep::Booked);
    job.filter(|job| booked & (job.state == JobState::Placed))
}

/// What `beat` does to `job`, placed on `allocation` (the rules of the module docs).
fn step_of(job: &JobRecord, allocation: &AllocationRecord, beat: &Beat<'_>) -> Step {
    let id = allocation.allocation_id.to_string();
    let ready = beat.ready == Some(id.as_str());
    let refused = beat.refused.filter(|refusal| refusal.lease_id == id);
    let left = (beat.phase != Phase::Preparing) | ready | refused.is_some();
    let refusal = refused.map_or(Step::Wait, |refusal| Step::Fail(refusal.code));
    match (
        job.cancel_requested,
        left,
        beat.first,
        ready & !beat.tainted,
    ) {
        (true, true, ..) => Step::Release,
        (true, false, ..) => Step::Wait,
        (false, _, true, _) => Step::Restart,
        (false, _, false, true) => Step::Start,
        (false, _, false, false) => refusal,
    }
}

/// When a prepare that begins at `now` must have ended: `prepare_timeout` later.
fn deadline(rules: &WholeNodeConfig, now: StoreTime) -> StoreTime {
    now.saturating_add(Duration::from_millis(rules.prepare_timeout_ms))
}

/// Applies what the heartbeat `beat` of a host says to the placed whole-host jobs the host
/// holds (the rules of the module docs) at `now`, under `rules`. A terminal failed prepare is
/// told to `metrics`. `taint` taints a host in the registry and answers whether that changed
/// anything; it is called for the host of a prepare the agent refused with a code that taints
/// it. Returns whether the registry's taint changed, so the caller persists it.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the agent reports again.
/// A job that moved between the read and the commit (a commit answered
/// [`JobStoreError::Stale`]) is such an error too: the heartbeat is answered 500, which writes
/// nothing more, and the agent's next report is judged against the job as it is then.
pub(crate) fn serve(
    jobs: &ServerJobs,
    beat: &Beat<'_>,
    (rules, now): (&WholeNodeConfig, StoreTime),
    (metrics, taint): (&dyn MetricsSink, &dyn Fn(&HostId) -> bool),
) -> Result<bool, JobStoreError> {
    let mut tainted = false;
    for (allocation, job) in jobs.live_on(beat.host)? {
        let Some(job) = placed_whole(&allocation, job) else {
            continue;
        };
        match step_of(&job, &allocation, beat) {
            Step::Release => cancel_and_free(jobs, &job)?,
            Step::Restart => {
                jobs.restart_prepare(&job.id, deadline(rules, now))?;
            }
            Step::Start => {
                jobs.transition(&job.id, JobState::Placed, JobState::Starting)?;
            }
            Step::Fail(code) => {
                let cause = PrepareCause::Refused(code);
                if fail_prepare(jobs, (&job, &allocation), cause, (rules, now), metrics)? {
                    tainted |= taint(beat.host);
                }
            }
            Step::Wait => {}
        }
    }
    Ok(tainted)
}

/// Fails the prepare of `job`, placed on `allocation`, for `cause` at `now`: one commit that
/// puts the job back in the queue (or ends it, after `max_replace` failed prepares), frees
/// its allocation and deletes its mark. A terminal one is told to `metrics`. Returns whether
/// the cause taints the host.
fn fail_prepare(
    jobs: &ServerJobs,
    (job, allocation): (&JobRecord, &AllocationRecord),
    cause: PrepareCause,
    (rules, now): (&WholeNodeConfig, StoreTime),
    metrics: &dyn MetricsSink,
) -> Result<bool, JobStoreError> {
    let (failed, taints) = failed_prepare(job, &allocation.host, cause, rules, now);
    let terminal = failed.terminal;
    let beside = |freed: &AllocationRecord| history(Some(job), freed);
    jobs.fail_prepare_with(&job.id, failed, &beside)?;
    if terminal {
        metrics.start_failed(StartFailure::ExclusiveNotReady);
    }
    Ok(taints)
}

/// Ends the prepare of each placed whole-host job whose allocation's prepare deadline is at or
/// before `now` (the module docs), and returns the jobs it ended, in the order of their
/// allocations: a job with a cancel intent is cancelled and freed, any other fails its prepare
/// with [`PrepareCause::Timeout`]. A job that moved on between the read and its commit (a
/// ready report, a cancel) is left for the next pass, which reads it again.
///
/// # Errors
/// The first [`JobStoreError`] of a read, or of a commit that failed for another reason than a
/// change to the job; what was committed before it stays.
pub(crate) fn expire_prepares(
    jobs: &ServerJobs,
    rules: &WholeNodeConfig,
    now: StoreTime,
    metrics: &dyn MetricsSink,
) -> Result<Vec<JobId>, JobStoreError> {
    let late = |record: &AllocationRecord| record.prepare_deadline.is_some_and(|at| at <= now);
    let mut ended = Vec::new();
    for allocation in jobs.live_allocations()?.into_iter().filter(late) {
        let Some(job) = placed_whole(&allocation, jobs.get_job(&allocation.job)?) else {
            continue;
        };
        let result = match job.cancel_requested {
            true => cancel_and_free(jobs, &job),
            false => fail_prepare(
                jobs,
                (&job, &allocation),
                PrepareCause::Timeout,
                (rules, now),
                metrics,
            )
            .map(|_taints| ()),
        };
        match result {
            Ok(()) => ended.push(job.id),
            Err(
                JobStoreError::Stale { .. }
                | JobStoreError::UnexpectedState { .. }
                | JobStoreError::IllegalTransition { .. },
            ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ended)
}

#[cfg(test)]
#[path = "job_whole_tests.rs"]
mod tests;
