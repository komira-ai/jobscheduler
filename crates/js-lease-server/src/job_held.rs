//! A start a host refused for a reason that is the host's and not the job's: the host is held for
//! maintenance (the agent's code `host-held`, [`js_agent_wire::refusal::HOST_HELD`]: it made
//! nothing for the job and cordons itself), it has no room for the job (`no-capacity`,
//! [`js_agent_wire::refusal::NO_CAPACITY`]: the job's booking does not fit what the host offers
//! less the bookings of the jobs it holds), or it is still fetching the bundle the job names
//! (`bundle-pending`, [`js_agent_wire::refusal::BUNDLE_PENDING`]: the first start of a bundle the
//! host does not hold begins its fetch, and the host refuses every start of that bundle until
//! the fetch ends). The job goes back to the queue rather than failing ([`held_or_end`]).
//!
//! A placed or starting job with no cancel asked for is requeued in one commit
//! ([`js_jobs::JobStore::requeue_held`]): its allocation is freed, the host is appended to the
//! job's held refusals (for `host-held`) or capacity refusals (for `no-capacity`), or to neither
//! (for `bundle-pending`), the job is kept off the host for [`HELD_BAR`], the pool's
//! `capacity_refusal_bar` or [`BUNDLE_PENDING_BAR`], and its queue status says why it waits
//! again. The held bar is the length of the cordon a held host keeps on itself, so the bar covers
//! the time before that cordon is in force, whatever placed the job. The bundle-pending bar is
//! short: the job is placed on another capable host meanwhile, or on the same host again once
//! the bar lapses, which then runs it if its fetch has ended. Nothing counts a requeue as
//! a failure: no start failure is told to the metrics, no failed prepare is counted and the host
//! is not tainted.
//!
//! A job a host refuses when [`MAX_HELD_REQUEUES`] held refusals, or the pool's
//! `capacity_requeues` capacity refusals, already sent it back ends as any refused start ends, failed to start with
//! [`js_jobs::StartFailure::AgentRefused`], with a detail that names every host that refused it
//! for that reason ([`named`]), so a fault that keeps answering the same code cannot keep a job
//! in the queue for ever. The two counts are kept apart: a held refusal does not count against
//! the capacity bound and the other way round. Bundle-pending refusals are not counted: a fetch
//! ends on its own (the agent then runs the job, or answers `bundle-fetch-failed`, which ends
//! it), and the job's queue deadline (the pool's `max_queue_wait`, from its submit, or for a
//! later attempt from the end of its retry's backoff) ends a job that waits longer. A cancelled job's refusal, and every other code (among them the bundle
//! refusals that asking again cannot change: `bundle-fetch-failed`, `bundle-not-allowed`,
//! `bundle-denied` and the rest), end the job as before.

use std::time::Duration;

use js_agent_wire::refusal;
use js_jobs::{
    AllocationRecord, Exclusion, HeldRequeue, JobEnd, JobRecord, JobState, JobStoreError, Needs,
    RequeuedFor, StartFailure,
};
use js_sched::queue_status::{self, Outcome};
use js_types::HostId;

use crate::cordon::MAX_CORDON;
use crate::job_alloc::history;
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;

/// How many times a start a held host refused sends a job back to the queue; the refusal
/// after that ends it.
pub(crate) const MAX_HELD_REQUEUES: usize = 3;

/// How long a job is kept off a host that refused its start as held: as long as the cordon a
/// held host places on itself and renews.
pub(crate) const HELD_BAR: Duration = MAX_CORDON;

/// How long a job is kept off a host that refused its start because it is still fetching the
/// job's bundle: long enough that the host is not sent the start again on every pass while it
/// fetches, short enough that the job runs soon after the fetch ends on a pool with that one
/// capable host.
pub(crate) const BUNDLE_PENDING_BAR: Duration = Duration::from_secs(30);

/// What becomes of `job`, placed on `host`, whose listed result reads as `end`: `None` when
/// `end` is a refusal `host-held`, `no-capacity` or `bundle-pending` of a placed or starting job
/// with no cancel asked for, which this call sent back to the queue (see the module docs);
/// otherwise the end to commit, which is `end` itself but for the refusal after
/// [`MAX_HELD_REQUEUES`] or `limits.capacity_requeues` requeues for its code, which names every
/// host that refused the job for that code.
///
/// # Errors
/// The [`JobStoreError`] of the clock's read or of the requeue: a row that changed since it
/// was read makes it [`JobStoreError::Stale`] (a cancel asked for in between, read by the
/// requeue, makes it [`JobStoreError::IllegalTransition`]); nothing is written, and the agent
/// reports again, its job then judged as the store holds it.
pub(crate) fn held_or_end(
    jobs: &ServerJobs,
    host: &HostId,
    job: &JobRecord,
    end: JobEnd,
    limits: &JobLimits,
) -> Result<Option<JobEnd>, JobStoreError> {
    let code = match &end {
        JobEnd::Refused { detail, .. } => detail.as_str(),
        _ => return Ok(Some(end)),
    };
    // Each code that requeues: the list its hosts are counted in with its bound (none for
    // bundle-pending), and how long the refusing host is barred.
    let (refused, counted, bar) = match code {
        refusal::HOST_HELD => (
            RequeuedFor::Held,
            Some((&job.held_refusals, MAX_HELD_REQUEUES)),
            HELD_BAR,
        ),
        refusal::NO_CAPACITY => (
            RequeuedFor::Capacity,
            Some((&job.capacity_refusals, limits.capacity_requeues)),
            limits.capacity_refusal_bar,
        ),
        refusal::BUNDLE_PENDING => (RequeuedFor::BundlePending, None, BUNDLE_PENDING_BAR),
        _ => return Ok(Some(end)),
    };
    let requeued = matches!(job.state, JobState::Placed | JobState::Starting);
    if !requeued | job.cancel_requested {
        return Ok(Some(end));
    }
    if let Some((earlier, _)) = counted.filter(|(earlier, bound)| earlier.len() >= *bound) {
        return Ok(Some(JobEnd::Refused {
            failure: StartFailure::AgentRefused,
            detail: named(code, earlier, host),
        }));
    }
    let until = jobs.now()?.saturating_add(bar);
    let mut prepare_bars = job.prepare_bars.clone();
    // A bar on the host the job is placed on no longer holds, or holds only as a back-off: the
    // refusal's bar replaces it, unless it is a bar that still ends later.
    let later = match prepare_bars.get(host) {
        Some(Exclusion::Until(end)) if *end > until => Exclusion::Until(*end),
        _ => Exclusion::Until(until),
    };
    prepare_bars.insert(host.clone(), later);
    let outcome = match refused {
        RequeuedFor::Held => Outcome::HeldRequeued {
            host: host.clone(),
            whole_host: job.spec.needs == Needs::WholeHost,
        },
        RequeuedFor::Capacity => Outcome::CapacityRequeued { host: host.clone() },
        RequeuedFor::BundlePending => Outcome::BundlePendingRequeued {
            host: host.clone(),
            whole_host: job.spec.needs == Needs::WholeHost,
        },
    };
    let requeue = HeldRequeue {
        refused,
        prepare_bars,
        queue: queue_status::note(&outcome, job.queue.code()),
    };
    let beside = |freed: &AllocationRecord| history(Some(job), freed);
    jobs.requeue_held(&job.id, requeue, &beside)?;
    Ok(None)
}

/// The detail of the refusal that ends a job hosts sent back too often: `code` and then each
/// host that refused the job for it, `earlier` and then `host`, once each in the order each
/// first refused it.
pub(crate) fn named(code: &str, earlier: &[HostId], host: &HostId) -> String {
    let mut hosts: Vec<&str> = Vec::new();
    for each in earlier.iter().chain([host]) {
        if !hosts.contains(&each.as_str()) {
            hosts.push(each.as_str());
        }
    }
    format!("{code}: {}", hosts.join(", "))
}

#[cfg(test)]
#[path = "job_held_tests.rs"]
mod tests;
