//! How a job becomes lost (design section 6.3): the two ways, and nothing else.
//!
//! 1. **The provider reads its host absent** ([`lose_absent`], step 2 of the tick, design
//!    section 6.1). The provider's word is read from the store's rows, so every server that
//!    claims the pass reads it alike, whichever server served the host (design
//!    `multi-instance.md`, G6): a host that holds a live allocation and has no `node/<host>`
//!    row is absent. Only a removal deletes such a row: the scaler's, once the cloud confirmed
//!    the machine gone ([`crate::scaler`]); a silent host's row is forgotten only once it holds
//!    nothing ([`js_jobs::JobStore::forget_silent_nodes`]). Every live (booked or releasing)
//!    allocation on the host is freed for [`Freed::ProviderAbsent`](crate::job_alloc::Freed),
//!    and its job, when it has not ended, becomes lost in the same commit
//!    ([`job_alloc::lose_on_absent`]). Each of those commits also checks that the host's row is
//!    as the pass read it (still absent), so a hello of the host that lands meanwhile makes the
//!    settlement a conflict: what the conflicting commit would have changed is kept, and so is
//!    every allocation of the host after it, which the host's first listing then judges. A job with a cancel intent is lost too, not cancelled:
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
//! **A lost host may retry the attempt** (`docs/design/job-retry-and-completion-callback.md`,
//! section 3.3). Each way asks [`crate::job_retry::next`] first: a placed, starting or running
//! job whose retry policy leaves an attempt after the current one, with no cancel asked for, is
//! not lost but queued for its next attempt ([`crate::job_retry::retry`], in one commit with
//! its allocation's move to releasing), its host barred for 15 minutes. On the first way the
//! allocation is then freed for the provider's absence, in a commit of its own: a pass that
//! stops between the two finds the allocation live on the absent host on its next read, and
//! frees it then, the job being queued. On the second way the host's listing frees it, as it
//! frees a lost job's. Every other job is lost as before.
//!
//! **Silence ends nothing.** No rule here, or anywhere in the job pass, compares how long a host
//! has been silent with a duration: a host that goes silent for good and is never read absent
//! keeps its jobs as they are and their allocations booked (design section 6.3; question 25,
//! option A, the accepted limit of section 3.2): its row stays, however long it is silent. A
//! clock-based ending of a silent host's jobs is the rejected option B or C of question 25; do
//! not add one here.

use std::collections::BTreeSet;

use js_jobs::{AllocationRecord, JobEnd, JobId, JobState, JobStoreError, NodeRow};
use js_types::HostId;
use tracing::info;

use crate::job_alloc::{self, Freed};
use crate::job_retry::{self, Next, Settled};
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

/// The row of a host as the job pass read it: the row with its version, none, or the error of
/// a read that failed (a row this build cannot read included).
pub(crate) type RowRead = Result<Option<NodeRow>, JobStoreError>;

/// Where the job pass reads what the provider says of a host.
pub(crate) trait Observe {
    /// What the provider reads of `host` now, given `row`, the host's `node/` row as the pass
    /// read it; an error is a read that failed, which counts as [`Seen::Unknown`].
    fn observe(&self, host: &HostId, row: &RowRead) -> Result<Seen, String>;
}

/// Settles the live allocations of each host `observer` reads absent (the first way of the
/// module docs), one commit per allocation (two for a job it retries: the retry, then the
/// free), and returns the jobs it made lost. Each host that holds a live allocation is read
/// once, its row first; only [`Seen::Absent`] of a row that could be read changes anything,
/// and every commit for the host checks its row is still as read (at its version, or absent).
/// A commit that finds the row moved ends the host's settlement for this pass and loses
/// nothing more of it; the other hosts are settled as usual. Each job it loses or retries is
/// told to `metrics` as its commit lands, with a loss whose retry policy had no attempt left.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit, other than the moved row; what was
/// committed before it stays, and the next pass reads the host again.
pub(crate) fn lose_absent(
    jobs: &ServerJobs,
    observer: &dyn Observe,
    metrics: &dyn MetricsSink,
) -> Result<Vec<JobId>, JobStoreError> {
    let live = jobs.live_allocations()?;
    let hosts: BTreeSet<&HostId> = live.iter().map(|allocation| &allocation.host).collect();
    let mut lost = Vec::new();
    for host in hosts {
        let Some(settling) = absent_view(jobs, observer, host) else {
            continue;
        };
        let on_host = live.iter().filter(|live| live.host == *host);
        match lose_each(&settling, on_host, metrics, &mut lost) {
            Err(JobStoreError::NodeMoved { .. }) => {
                info!(%host, "the host's row came back while its jobs were settled; the rest are kept");
            }
            settled => settled?,
        }
    }
    Ok(lost)
}

/// The view of `jobs` the allocations of `host` are settled through when `observer` reads it
/// absent: every write checks the host's row is as read now (at its version, or absent);
/// `None` when the host is not read absent, or its row could not be read.
fn absent_view(jobs: &ServerJobs, observer: &dyn Observe, host: &HostId) -> Option<ServerJobs> {
    let row = jobs.read_node(host);
    let absent = matches!(observer.observe(host, &row), Ok(Seen::Absent));
    let row = row.ok().filter(|_| absent)?;
    Some(row.map_or_else(
        || jobs.guarded_absent(host),
        |row| jobs.guarded(host, row.version),
    ))
}

/// Loses or retries the job of each of `allocations`, all on one host, through `jobs` (the
/// host's guarded view), adding each job it made lost to `lost`.
///
/// # Errors
/// The first [`JobStoreError`] of a read or a commit, [`JobStoreError::NodeMoved`] included.
fn lose_each<'a>(
    jobs: &ServerJobs,
    allocations: impl Iterator<Item = &'a AllocationRecord>,
    metrics: &dyn MetricsSink,
    lost: &mut Vec<JobId>,
) -> Result<(), JobStoreError> {
    for allocation in allocations {
        let job = jobs.get_job(&allocation.job)?;
        let next = job.as_ref().map(|job| job_retry::next(job, &JobEnd::Lost));
        if let (Some(job), Some(Next::Retry { wait })) = (&job, next) {
            job_retry::retry(jobs, job, &JobEnd::Lost, wait, metrics)?;
            job_alloc::free_allocation(jobs, &allocation.job, Freed::ProviderAbsent)?;
            continue;
        }
        let exhausted = matches!(next, Some(Next::End { exhausted: true }));
        let ended = job_alloc::lose_on_absent(jobs, &allocation.job)?;
        lost.extend(ended.map(|job| {
            metrics.job_lost();
            if exhausted {
                metrics.attempts_exhausted();
            }
            job.id
        }));
    }
    Ok(())
}

/// The jobs of `host` that are starting or running and that its listing `held` (the ids the
/// host listed or retired) does not name, each lost, or retried when its retry policy leaves
/// another attempt, in one commit with its allocation's move to releasing (the second way of
/// the module docs); the lost ones are returned in id order. Called on the first listing of the
/// host after its hello. Each job it loses or retries is told to `metrics` as its commit lands.
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
        let ran = job
            .as_ref()
            .is_some_and(|job| matches!(job.state, JobState::Starting | JobState::Running));
        let Some(job) = job.filter(|_| ran && !held.contains(allocation.job.as_str())) else {
            continue;
        };
        if let Settled::Ended(_) = job_retry::end_or_retry(jobs, &job, JobEnd::Lost, metrics)? {
            metrics.job_lost();
            lost.push(allocation.job);
        }
    }
    Ok(lost)
}

#[cfg(test)]
#[path = "job_loss_tests.rs"]
mod tests;
