//! How a job becomes lost (design section 6.3): the two ways, and nothing else.
//!
//! 1. **The provider reads its host absent** ([`lose_absent`], step 2 of the tick, design
//!    section 6.1). For an elastic machine that is an explicit not-found; for a registered
//!    host it is the registry removing the host. Every live (booked or releasing) allocation
//!    on the host is freed for [`Freed::ProviderAbsent`](crate::job_alloc::Freed), and its
//!    job, when it has not ended, becomes lost in the same commit
//!    ([`job_alloc::lose_on_absent`]). A job with a cancel intent is lost too, not cancelled:
//!    no host report exists, and its record keeps the intent, which `GetJob` shows. A job that
//!    had already ended (a start timeout waiting for its host's confirmation) keeps its end and
//!    only its allocation is freed. An unknown read, a read that failed, and a host that is only
//!    silent change nothing.
//! 2. **The host comes back and does not list the job** ([`lose_unlisted`]). On the first job
//!    listing of a host after its hello, each of its jobs that is starting or running and that
//!    the listing neither lists nor retires is lost: the container went with the agent (design
//!    section 6.7, step 6). Its allocation moves to releasing in the same commit, and is freed
//!    when the host confirms the container gone, which the same listing does
//!    ([`crate::per_job`]). A placed job the listing omits is not lost: its start is due again
//!    and rides on the reply. A job the listing names keeps its state.
//!
//! Lost is terminal: a host that later lists a lost job is told to stop it and the job is never
//! revived ([`crate::per_job::stops_for`]).
//!
//! **Silence ends nothing.** No rule here, or anywhere in the job pass, compares how long a host
//! has been silent with a duration: a host that goes silent for good and is never read absent
//! keeps its jobs as they are and their allocations booked (design section 6.3; question 25,
//! option A, the accepted limit of section 3.2). A clock-based ending of a silent host's jobs is
//! the rejected option B or C of question 25; do not add one here.

use std::collections::BTreeSet;

use js_jobs::{JobEnd, JobId, JobState, JobStoreError};
use js_types::HostId;

use crate::job_alloc;
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;

/// What the machine provider reads of a host (design section 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The host is there.
    Present,
    /// The provider says the host is gone: an explicit not-found, or the host removed.
    Absent,
    /// The provider cannot say.
    Unknown,
}

/// Where the job pass reads what the provider says of a host.
pub(crate) trait Observe {
    /// What the provider reads of `host` now; an error is a read that failed, which counts as
    /// [`Seen::Unknown`].
    fn observe(&self, host: &HostId) -> Result<Seen, String>;
}

/// Settles the live allocations of each host `observer` reads absent (the first way of the
/// module docs), one commit per allocation, and returns the jobs it made lost. Each host that
/// holds a live allocation is read once; only [`Seen::Absent`] changes anything. Each job it
/// loses is told to `metrics` as its commit lands.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the next pass reads the host again.
pub(crate) fn lose_absent(
    jobs: &ServerJobs,
    observer: &dyn Observe,
    metrics: &dyn MetricsSink,
) -> Result<Vec<JobId>, JobStoreError> {
    let live = jobs.live_allocations()?;
    let hosts: BTreeSet<&HostId> = live.iter().map(|allocation| &allocation.host).collect();
    let absent: BTreeSet<&HostId> = hosts
        .into_iter()
        .filter(|host| matches!(observer.observe(host), Ok(Seen::Absent)))
        .collect();
    let mut lost = Vec::new();
    for allocation in live.iter().filter(|live| absent.contains(&live.host)) {
        let ended = job_alloc::lose_on_absent(jobs, &allocation.job)?;
        lost.extend(ended.map(|job| {
            metrics.job_lost();
            job.id
        }));
    }
    Ok(lost)
}

/// The jobs of `host` that are starting or running and that its listing `held` (the ids the
/// host listed or retired) does not name, each lost in one commit with its allocation's move to
/// releasing (the second way of the module docs), returned in id order. Called on the first
/// listing of the host after its hello. Each job it loses is told to `metrics` as its commit
/// lands.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit; what was committed before it stays, and
/// the host's next listing is the first again.
pub(crate) fn lose_unlisted(
    jobs: &ServerJobs,
    host: &HostId,
    held: &BTreeSet<&str>,
    metrics: &dyn MetricsSink,
) -> Result<Vec<JobId>, JobStoreError> {
    let mut lost = Vec::new();
    for (allocation, job) in jobs.live_on(host)? {
        let ran =
            job.is_some_and(|job| matches!(job.state, JobState::Starting | JobState::Running));
        if ran && !held.contains(allocation.job.as_str()) {
            jobs.end_job(&allocation.job, JobEnd::Lost)?;
            metrics.job_lost();
            lost.push(allocation.job);
        }
    }
    Ok(lost)
}

#[cfg(test)]
#[path = "job_loss_tests.rs"]
mod tests;
