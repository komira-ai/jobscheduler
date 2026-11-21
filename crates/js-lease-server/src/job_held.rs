//! A start that a held host refused (the agent's code `host-held`,
//! [`js_agent_wire::refusal::HOST_HELD`]): the host is held for maintenance, made nothing for
//! the job and cordons itself, so the job goes back to the queue rather than failing
//! ([`held_or_end`]).
//!
//! A placed or starting job with no cancel asked for is requeued in one commit
//! ([`js_jobs::JobStore::requeue_held`]): its allocation is freed, the host is appended to the
//! job's held refusals, the job is kept off the host for [`HELD_BAR`] (the length of the
//! cordon a held host keeps on itself, so the bar covers the time before that cordon is in
//! force, whatever placed the job), and its queue status says why it waits again. Nothing
//! counts it as a failure: no start failure is told to the metrics, no failed prepare is
//! counted and the host is not tainted.
//!
//! A job a held host refuses when [`MAX_HELD_REQUEUES`] such refusals already sent it back
//! ends as any refused start ends, failed to start with
//! [`js_jobs::StartFailure::AgentRefused`], with a detail that names every host that refused
//! it ([`named`]), so a fault that keeps answering `host-held` cannot keep a job in the queue
//! for ever. A cancelled job's refusal, and every other code, end the job as before.

use std::time::Duration;

use js_agent_wire::refusal;
use js_jobs::{
    AllocationRecord, Exclusion, HeldRequeue, JobEnd, JobRecord, JobState, JobStoreError, Needs,
    StartFailure,
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

/// What becomes of `job`, placed on `host`, whose listed result reads as `end`: `None` when
/// `end` is a refusal `host-held` of a placed or starting job with no cancel asked for, which
/// this call sent back to the queue (see the module docs); otherwise the end to commit,
/// which is `end` itself but for the refusal after [`MAX_HELD_REQUEUES`] requeues, which names
/// every host that refused the job.
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
    let held = matches!(&end, JobEnd::Refused { detail, .. } if detail == refusal::HOST_HELD);
    let requeued = matches!(job.state, JobState::Placed | JobState::Starting);
    if !held | !requeued | job.cancel_requested {
        return Ok(Some(end));
    }
    if job.held_refusals.len() >= MAX_HELD_REQUEUES {
        return Ok(Some(JobEnd::Refused {
            failure: StartFailure::AgentRefused,
            detail: named(&job.held_refusals, host),
        }));
    }
    let until = jobs.now()?.saturating_add(HELD_BAR);
    let mut prepare_bars = job.prepare_bars.clone();
    // A bar on the host the job is placed on no longer holds, or holds only as a back-off: the
    // hold's bar replaces it.
    prepare_bars.insert(host.clone(), Exclusion::Until(until));
    let whole_host = job.spec.needs == Needs::WholeHost;
    let outcome = Outcome::HeldRequeued {
        host: host.clone(),
        whole_host,
    };
    let requeue = HeldRequeue {
        prepare_bars,
        queue: queue_status::note(&outcome, job.queue.code()),
    };
    let beside = |freed: &AllocationRecord| history(Some(job), freed);
    jobs.requeue_held(&job.id, requeue, &beside)?;
    Ok(None)
}

/// The detail of the refusal that ends a job held hosts sent back too often: the code and then
/// each host that refused the job, `earlier` and then `host`, once each in the order each first
/// refused it.
pub(crate) fn named(earlier: &[HostId], host: &HostId) -> String {
    let mut hosts: Vec<&str> = Vec::new();
    for each in earlier.iter().chain([host]) {
        if !hosts.contains(&each.as_str()) {
            hosts.push(each.as_str());
        }
    }
    format!("{}: {}", refusal::HOST_HELD, hosts.join(", "))
}

#[cfg(test)]
#[path = "job_held_tests.rs"]
mod tests;
