//! How a job becomes lost (design section 6.3): the three ways, and nothing else.
//!
//! 1. **The provider reads its host absent** ([`lose_absent`], step 2 of the tick, design
//!    section 6.1). The provider's word is read from the store's rows, so every server that
//!    claims the pass reads it alike, whichever server served the host (design
//!    `multi-instance.md`, G6): a host that holds a live allocation and has no `node/<host>`
//!    row is absent. Only a removal deletes such a row: the scaler's, once the cloud confirmed
//!    the machine gone ([`crate::scaler`]), or the operator's retirement of a lost host that
//!    owes only the unconfirmed stops of ended jobs ([`crate::agents::Agents::retire`], which
//!    frees those itself); a silent host's row is forgotten only once it holds nothing
//!    ([`js_jobs::JobStore::forget_silent_nodes`]). Every live (booked or releasing)
//!    allocation on the host is freed for [`Freed::ProviderAbsent`](crate::job_alloc::Freed),
//!    and its job, when it has not ended, becomes lost in the same commit
//!    ([`job_alloc::lose_on_absent`]). Each of those commits also checks that the host's row is
//!    as the pass read it (still absent), so a hello of the host that lands meanwhile makes the
//!    settlement a conflict: what the conflicting commit would have changed is kept, and so is
//!    every allocation of the host after it, which the host's first listing then judges. A job with a cancel intent is lost too, not cancelled:
//!    no host report exists, and its record keeps the intent, which `GetJob` shows. A job that
//!    had already ended (a start timeout waiting for its host's confirmation, or a job lost to
//!    the host's silence) keeps its end and only its allocation is freed. An unknown read and a
//!    read that failed change nothing.
//! 2. **The host comes back and does not list the job** ([`lose_unlisted`]). On the first job
//!    listing of a host after its hello, each of its jobs that is starting or running and that
//!    the listing neither lists nor retires is lost: the container went with the agent (design
//!    section 6.7, step 6). Its allocation moves to releasing in the same commit, and is freed
//!    when the host confirms the container gone, which the same listing does
//!    ([`crate::per_job`]). A placed job the listing omits is not lost: its start is due again
//!    and rides on the reply. A job the listing names keeps its state.
//! 3. **The host is silent past the server's `silence_limit`** ([`lose_silent`], step 3 of the
//!    tick). A host whose `node/` row was last heard (a hello or an applied beat, stamped by the
//!    store's clock) `silence_limit` or more before the pass's `now` has each of its placed,
//!    starting and running jobs (a cancel asked for or not) ended as lost with the cause
//!    [`LostCause::HostSilent`], which `GetJob` shows, its allocation moved to releasing in the
//!    same commit ([`js_jobs::JobStore::end_job`]). The silence counts from the later of the
//!    row's last hearing and `floor`, when this server's job pass last began to read the store
//!    after a boot or a failed pass, so an outage of the servers or of the store is never
//!    counted as the hosts' silence. Each end is conditional on the host's row at the version
//!    read: a beat that lands in between makes it a conflict that ends nothing more on the host.
//!
//!    Such a job is **never retried** by the server, whatever its retry policy, and its
//!    allocation is **not freed**: the host's room stays booked, so nothing else is placed in
//!    it, until the host confirms the container gone (it is told to stop the job on each reply
//!    while it lists it, [`crate::per_job::stops_for`], and a stop is queued for a host that
//!    polls), the provider reads the host absent (the first way), or the operator retires the
//!    host ([`crate::agents::Agents::retire`]). No job runs twice for it: the server places
//!    nothing new for the job, a host that comes back is told to stop it and never to start
//!    it, and an agent cut off from the server stops its own jobs once it has not been
//!    answered for its `--silence-stop` (300 s by default), far inside the limit. A client
//!    that wants the work done again submits a new job.
//!
//! Lost is terminal: a host that later lists a lost job is told to stop it and the job is never
//! revived ([`crate::per_job::stops_for`]); a result it reports for it is acknowledged and
//! dropped.
//!
//! **A lost host may retry the attempt** (`docs/design/job-retry-and-completion-callback.md`,
//! section 3.3). The first two ways ask [`crate::job_retry::next`] first: a placed, starting or
//! running job whose retry policy leaves an attempt after the current one, with no cancel asked
//! for, is not lost but queued for its next attempt ([`crate::job_retry::retry`], in one commit
//! with its allocation's move to releasing), its host barred for 15 minutes. On the first way
//! the allocation is then freed for the provider's absence, in a commit of its own: a pass that
//! stops between the two finds the allocation live on the absent host on its next read, and
//! frees it then, the job being queued. On the second way the host's listing frees it, as it
//! frees a lost job's. Every other job is lost as before. The third way never retries: the
//! attempt may still run on the host, and a retry would place it a second time.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_jobs::{
    AllocationRecord, JobEnd, JobId, JobRecord, JobState, JobStoreError, LostCause, NodeRow,
};
use js_state::StoreTime;
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

/// Ends as lost, with the cause [`LostCause::HostSilent`] and no retry, each placed, starting or
/// running job with a live allocation on a host silent for `limit` or longer at `now` (the
/// third way of the module docs), each in one commit with its allocation's move to releasing,
/// conditional on the host's row at the version read; returns the jobs it ended, host by host
/// in host order and each host's in id order. A host is silent when the later of its row's
/// `heard_at` and `floor` is `limit` or more before `now`; a host with no readable row is not
/// judged. A beat that lands on a host's row before one of its ends commits leaves that job and
/// the host's later ones as they are; a job another writer moved or ended in between keeps what
/// it has. Each job it ends is told to `metrics` as its commit lands.
///
/// # Errors
/// The first [`JobStoreError`] of a read, or of a commit that failed for another reason than a
/// change to the job or the host's row; what was committed before it stays.
pub(crate) fn lose_silent(
    jobs: &ServerJobs,
    (limit, floor): (Duration, StoreTime),
    now: StoreTime,
    metrics: &dyn MetricsSink,
) -> Result<Vec<JobId>, JobStoreError> {
    let holding: BTreeSet<HostId> = jobs
        .live_allocations()?
        .into_iter()
        .map(|allocation| allocation.host)
        .collect();
    let (_, rows) = jobs.nodes()?;
    let silent: BTreeMap<HostId, _> = rows
        .into_iter()
        .filter(|row| holding.contains(&row.record.host))
        .filter(|row| row.record.heard_at.max(floor).saturating_add(limit) <= now)
        .map(|row| (row.record.host, row.version))
        .collect();
    let mut lost = Vec::new();
    for (host, version) in silent {
        let guarded = jobs.guarded(&host, version);
        for (_, job) in jobs.live_on(&host)? {
            let Some(job) = job.filter(live) else {
                continue;
            };
            match guarded.end_job(&job.id, JobEnd::HostSilent) {
                Ok(ended) if ended.lost_cause == Some(LostCause::HostSilent) => {
                    metrics.job_lost();
                    lost.push(job.id);
                }
                Ok(_)
                | Err(JobStoreError::IllegalTransition { .. } | JobStoreError::Stale { .. }) => {}
                // The host was heard since its row was read: it is not silent.
                Err(JobStoreError::NodeMoved { .. }) => break,
                Err(error) => return Err(error),
            }
        }
    }
    Ok(lost)
}

/// Whether `job` is placed, starting or running: one the third way ends.
fn live(job: &JobRecord) -> bool {
    matches!(
        job.state,
        JobState::Placed | JobState::Starting | JobState::Running
    )
}

#[cfg(test)]
#[path = "job_loss_tests.rs"]
mod tests;
