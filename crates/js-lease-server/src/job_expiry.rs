//! The time rules of the job pass (design sections 3.2, "What bounds a job", and 6.1, step
//! 1): a queued job that waits past its pool's `max_queue_wait` fails to start with
//! `no-capacity`, and a placed or starting job that has not started within its pool's
//! `start_timeout` fails to start with `start-timeout`, except a placed whole-host job, which
//! its prepare deadline bounds instead (see the table below).
//!
//! Each limit is counted from an instant the job's record stores, never from the boot or from
//! the last pass: the queue wait from when the job's current attempt became placeable (its
//! submit for the first attempt, the end of the backoff its retry set for a later one:
//! `docs/design/job-retry-and-completion-callback.md`, section 3.4), the start timeout from its
//! placement commit. Each deadline is also held to at least the job's deadline floor, which
//! a boot raises to the boot plus 120 s ([`js_jobs::JobStore::raise_at_boot`], see
//! [`crate::job_pass`]), so an outage longer than a limit fails no job at the first tick
//! after it:
//!
//! | state | deadline |
//! |---|---|
//! | queued | `max(placeable_from + max_queue_wait, deadline_floor)`, `placeable_from` the submit or the retry's not-before |
//! | placed, starting | `max(placed_at + start_timeout, deadline_floor)` |
//! | placed, whole host | none: its prepare deadline governs it ([`crate::job_whole`]) |
//! | starting, whole host | `max(prepare_deadline + start_timeout, deadline_floor)` |
//!
//! A whole-host job has no start to time until its host is prepared: it is placed while the
//! agent prepares, and the prepare deadline of its allocation ends that wait (the tick's
//! [`crate::job_whole::expire_prepares`]). Once the host reports ready the job is starting,
//! and its start counts from that deadline, the latest instant the report could have come
//! (the placement's time when its allocation records none). A queued whole-host job that
//! waits past its queue wait ends with its mark deleted in the same commit.
//!
//! A job ends at its deadline, not after it, except that a start that timed out retries when
//! the job's retry policy leaves another attempt ([`crate::job_retry`]): the job is queued for
//! its next attempt instead, in one commit with its allocation's move to releasing, and its
//! host barred for 15 minutes. A queue wait that runs out never retries. A running job is
//! never touched here: its wall
//! limit is counted by its agent (design section 5.7). The end of a placed or starting job
//! moves its allocation to releasing in the same commit ([`js_jobs::JobStore::end_job`]);
//! the host is told to stop the job on each heartbeat reply while it still lists it
//! ([`crate::per_job::stops_for`]), and the allocation is freed only when the host confirms
//! the container gone ([`crate::job_alloc`]). Nothing here reads a clock: `now` is given.

use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobRecord, JobState, JobStoreError, Needs, StartFailure};
use js_state::StoreTime;

use crate::file::PoolSettings;
use crate::job_api::submit::DEFAULT_MAX_JOB_WALL;
use crate::job_retry::{self, Settled};
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;

/// How long a pool's jobs may wait (design section 4.2): its `max_queue_wait` and its
/// `start_timeout`; and how a start a host refused for capacity is answered: its
/// `capacity_requeues` and `capacity_refusal_bar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobLimits {
    /// How long a queued job waits for a host before it ends as `no-capacity`.
    pub(crate) max_queue_wait: Duration,
    /// How long a placed or starting job has to start before it ends as `start-timeout`.
    pub(crate) start_timeout: Duration,
    /// How many times a start a host refused for capacity sends a job back to the queue; the
    /// refusal after that ends it.
    pub(crate) capacity_requeues: usize,
    /// How long a job is kept off a host that refused its start for capacity.
    pub(crate) capacity_refusal_bar: Duration,
    /// How long a host lists a job the store holds no record of before the server ends it
    /// ([`crate::orphans`]): the most any job may run (`--max-job-wall`), the same for every
    /// pool.
    pub(crate) orphan_after: Duration,
}

impl JobLimits {
    /// The limits of a pool that sets none: a queue wait of one hour, a start timeout of
    /// fifteen minutes, eight capacity requeues and a capacity bar of sixty seconds; an
    /// orphan is ended after the default `--max-job-wall`.
    pub(crate) const DEFAULT: JobLimits = JobLimits {
        max_queue_wait: Duration::from_secs(3600),
        start_timeout: Duration::from_secs(900),
        capacity_requeues: 8,
        capacity_refusal_bar: Duration::from_secs(60),
        orphan_after: DEFAULT_MAX_JOB_WALL,
    };

    /// The limits the pool's `settings` give, an orphan ended after the default
    /// `--max-job-wall` ([`JobLimits::with_orphan_after`] sets the server's).
    pub(crate) fn of(settings: &PoolSettings) -> JobLimits {
        JobLimits {
            max_queue_wait: settings.max_queue_wait,
            start_timeout: settings.start_timeout,
            capacity_requeues: settings.capacity_requeues,
            capacity_refusal_bar: settings.capacity_refusal_bar,
            orphan_after: DEFAULT_MAX_JOB_WALL,
        }
    }

    /// The same limits, an orphan ended after `max_job_wall`, the server's `--max-job-wall`.
    pub(crate) fn with_orphan_after(self, max_job_wall: Duration) -> JobLimits {
        JobLimits {
            orphan_after: max_job_wall,
            ..self
        }
    }
}

impl Default for JobLimits {
    fn default() -> JobLimits {
        JobLimits::DEFAULT
    }
}

/// When `job` runs out of time under `limits`, and how it then fails to start: for a queued
/// job its queue-wait deadline, for a placed or starting one its start deadline (see the
/// module docs), each held to at least its deadline floor; `None` for a job in any other
/// state, which no limit here ends, and for a placed whole-host job, which the prepare
/// deadline governs. `prepared` is the prepare deadline of the job's allocation, when it
/// records one: a starting job's start counts from it.
pub(crate) fn deadline(
    job: &JobRecord,
    prepared: Option<StoreTime>,
    limits: JobLimits,
) -> Option<(StoreTime, StartFailure)> {
    let whole = matches!(job.spec.needs, Needs::WholeHost);
    let (from, limit, failure) = match job.state {
        JobState::Queued => (
            job.attempt.retry_not_before().unwrap_or(job.submitted_at),
            limits.max_queue_wait,
            StartFailure::NoCapacity,
        ),
        JobState::Placed if whole => return None,
        // A placed job always has its placement time; a record without one counts from its
        // submit, the earlier instant.
        JobState::Placed | JobState::Starting => (
            prepared.or(job.placed_at).unwrap_or(job.submitted_at),
            limits.start_timeout,
            StartFailure::StartTimeout,
        ),
        _ => return None,
    };
    let natural = from.saturating_add(limit);
    let at = job
        .deadline_floor
        .map_or(natural, |floor| natural.max(floor));
    Some((at, failure))
}

/// How `job` (whose allocation records the prepare deadline `prepared`, if any) fails to start
/// at `now` under `limits`, when its deadline is at or before `now`; `None` while it is not
/// out of time, or when no limit here ends it.
pub(crate) fn due(
    job: &JobRecord,
    prepared: Option<StoreTime>,
    limits: JobLimits,
    now: StoreTime,
) -> Option<StartFailure> {
    deadline(job, prepared, limits)
        .filter(|(at, _)| now >= *at)
        .map(|(_, failure)| failure)
}

/// Ends each job of `jobs` that is out of time at `now` under `limits`, each in one commit
/// with its allocation's move to releasing (a queued whole-host job's with its mark's delete),
/// or retries it when its start timed out and its retry policy leaves another attempt, and
/// returns those it ended with how. The jobs read are the queued ones and those of every live
/// allocation that is not queued (a placed or starting job always has one; a queued job holds
/// one only while its previous attempt's is released, and is read once, as queued), each with
/// its allocation's prepare deadline. A job that moved on between the read and its commit (placed, started,
/// ended, or changed by a heartbeat) is left for the next pass, which reads it again; a job
/// that another writer ended in between keeps that end and is not returned. Each job it ends
/// is told to `metrics` as its commit lands, so a pass that fails later still counts it.
///
/// # Errors
/// The first [`JobStoreError`] of a read, or of a commit that failed for another reason than a
/// change to the job; what was committed before it stays.
pub(crate) fn expire(
    jobs: &ServerJobs,
    limits: JobLimits,
    now: StoreTime,
    metrics: &dyn MetricsSink,
) -> Result<Vec<(JobId, StartFailure)>, JobStoreError> {
    let mut waiting: Vec<(JobRecord, Option<StoreTime>)> =
        jobs.queued()?.into_iter().map(|job| (job, None)).collect();
    for allocation in jobs.live_allocations()? {
        let prepared = allocation.prepare_deadline;
        let job = jobs.get_job(&allocation.job)?;
        let job = job.filter(|job| job.state != JobState::Queued);
        waiting.extend(job.map(|job| (job, prepared)));
    }
    let mut ended = Vec::new();
    for (job, prepared) in waiting {
        let Some(failure) = due(&job, prepared, limits, now) else {
            continue;
        };
        let end = JobEnd::FailedToStart(failure);
        let unmark = (job.state == JobState::Queued) & matches!(job.spec.needs, Needs::WholeHost);
        let result = match unmark {
            true => jobs.end_and_unmark(&job.id, end),
            false => match job_retry::end_or_retry(jobs, &job, end, metrics) {
                Ok(Settled::Ended(record)) => Ok(record),
                Ok(Settled::Retried(_)) => continue,
                Err(error) => Err(error),
            },
        };
        match result {
            Ok(record) => {
                let by_this = record.state == JobState::FailedToStart(failure);
                ended.extend(by_this.then(|| {
                    metrics.start_failed(failure);
                    (job.id, failure)
                }));
            }
            Err(JobStoreError::IllegalTransition { .. } | JobStoreError::Stale { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ended)
}

#[cfg(test)]
#[path = "job_expiry_tests.rs"]
mod tests;
