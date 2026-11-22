//! A start a host refused for a reason that is the host's and not the job's: the host is held
//! for maintenance (the agent's code `host-held`, [`js_agent_wire::refusal::HOST_HELD`]: it made
//! nothing for the job and cordons itself) or it has no room for the job (`no-capacity`,
//! [`js_agent_wire::refusal::NO_CAPACITY`]: the job's booking does not fit what the host offers
//! less the bookings of the jobs it holds). The job goes back to the queue rather than failing
//! ([`held_or_end`]).
//!
//! A placed or starting job with no cancel asked for is requeued in one commit
//! ([`js_jobs::JobStore::requeue_held`]): its allocation is freed, the host is appended to the
//! job's held refusals (for `host-held`) or capacity refusals (for `no-capacity`), the job is
//! kept off the host for [`HELD_BAR`] or [`CAPACITY_BAR`], and its queue status says why it waits
//! again. The held bar is the length of the cordon a held host keeps on itself, so the bar covers
//! the time before that cordon is in force, whatever placed the job. Nothing counts a requeue as
//! a failure: no start failure is told to the metrics, no failed prepare is counted and the host
//! is not tainted.
//!
//! A job a host refuses when [`MAX_HELD_REQUEUES`] held refusals, or [`MAX_CAPACITY_REQUEUES`]
//! capacity refusals, already sent it back ends as any refused start ends, failed to start with
//! [`js_jobs::StartFailure::AgentRefused`], with a detail that names every host that refused it
//! for that reason ([`named`]), so a fault that keeps answering the same code cannot keep a job
//! in the queue for ever. The two counts are kept apart: a held refusal does not count against
//! the capacity bound and the other way round. A cancelled job's refusal, and every other code,
//! end the job as before.

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
use crate::job_store::ServerJobs;

/// How many times a start a held host refused sends a job back to the queue; the refusal
/// after that ends it.
pub(crate) const MAX_HELD_REQUEUES: usize = 3;

/// How long a job is kept off a host that refused its start as held: as long as the cordon a
/// held host places on itself and renews.
pub(crate) const HELD_BAR: Duration = MAX_CORDON;

/// How many times a start a host refused for capacity sends a job back to the queue; the
/// refusal after that ends it. The job service packs each host by the rule its agent enforces,
/// so a capacity refusal means the agent's view of its room differs from the server's (bookings
/// not yet freed on one side, containers the agent could not remove). Eight refusals give every
/// host of a small farm a chance and a second look after its bar; a job refused that often by
/// hosts that each said no is one the farm does not run as it is, so it ends failed to start,
/// naming the hosts. The queue wait bounds its time in the queue in any case.
pub(crate) const MAX_CAPACITY_REQUEUES: usize = 8;

/// How long a job is kept off a host that refused its start for capacity: long enough to keep
/// the job off the host that just said it has no room while its earlier jobs end, and short
/// because jobs end within minutes and the host is then fit again.
pub(crate) const CAPACITY_BAR: Duration = Duration::from_secs(60);

/// What becomes of `job`, placed on `host`, whose listed result reads as `end`: `None` when
/// `end` is a refusal `host-held` or `no-capacity` of a placed or starting job with no cancel
/// asked for, which this call sent back to the queue (see the module docs); otherwise the end
/// to commit, which is `end` itself but for the refusal after [`MAX_HELD_REQUEUES`] or
/// [`MAX_CAPACITY_REQUEUES`] requeues for its code, which names every host that refused the job
/// for that code.
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
) -> Result<Option<JobEnd>, JobStoreError> {
    let code = match &end {
        JobEnd::Refused { detail, .. } => detail.as_str(),
        _ => return Ok(Some(end)),
    };
    let (refused, earlier, bound, bar) = match code {
        refusal::HOST_HELD => (
            RequeuedFor::Held,
            &job.held_refusals,
            MAX_HELD_REQUEUES,
            HELD_BAR,
        ),
        refusal::NO_CAPACITY => (
            RequeuedFor::Capacity,
            &job.capacity_refusals,
            MAX_CAPACITY_REQUEUES,
            CAPACITY_BAR,
        ),
        _ => return Ok(Some(end)),
    };
    let requeued = matches!(job.state, JobState::Placed | JobState::Starting);
    if !requeued | job.cancel_requested {
        return Ok(Some(end));
    }
    if earlier.len() >= bound {
        return Ok(Some(JobEnd::Refused {
            failure: StartFailure::AgentRefused,
            detail: named(code, earlier, host),
        }));
    }
    let until = jobs.now()?.saturating_add(bar);
    let mut prepare_bars = job.prepare_bars.clone();
    // A bar on the host the job is placed on no longer holds, or holds only as a back-off: the
    // refusal's bar replaces it.
    prepare_bars.insert(host.clone(), Exclusion::Until(until));
    let outcome = match refused {
        RequeuedFor::Held => Outcome::HeldRequeued {
            host: host.clone(),
            whole_host: job.spec.needs == Needs::WholeHost,
        },
        RequeuedFor::Capacity => Outcome::CapacityRequeued { host: host.clone() },
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
