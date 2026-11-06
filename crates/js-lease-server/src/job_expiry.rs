//! The time rules of the job pass (design sections 3.2, "What bounds a job", and 6.1, step
//! 1): a queued job that waits past its pool's `max_queue_wait` fails to start with
//! `no-capacity`, and a placed or starting job that has not started within its pool's
//! `start_timeout` fails to start with `start-timeout`.
//!
//! Each limit is counted from an instant the job's record stores, never from the boot or from
//! the last pass: the queue wait from when the job was queued, the start timeout from its
//! placement commit. Each deadline is also held to at least the job's deadline floor, which
//! a boot raises to the boot plus 120 s ([`js_jobs::JobStore::raise_at_boot`], see
//! [`crate::job_pass`]), so an outage longer than a limit fails no job at the first tick
//! after it:
//!
//! | state | deadline |
//! |---|---|
//! | queued | `max(queued_at + max_queue_wait, deadline_floor)` |
//! | placed, starting | `max(placed_at + start_timeout, deadline_floor)` |
//!
//! A job ends at its deadline, not after it. A running job is never touched here: its wall
//! limit is counted by its agent (design section 5.7). The end of a placed or starting job
//! moves its allocation to releasing in the same commit ([`js_jobs::JobStore::end_job`]);
//! the host is told to stop the job on each heartbeat reply while it still lists it
//! ([`crate::per_job::stops_for`]), and the allocation is freed only when the host confirms
//! the container gone ([`crate::job_alloc`]). Nothing here reads a clock: `now` is given.

use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobRecord, JobState, JobStoreError, StartFailure};
use js_state::StoreTime;

use crate::file::PoolSettings;
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;

/// How long a pool's jobs may wait (design section 4.2): its `max_queue_wait` and its
/// `start_timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobLimits {
    /// How long a queued job waits for a host before it ends as `no-capacity`.
    pub(crate) max_queue_wait: Duration,
    /// How long a placed or starting job has to start before it ends as `start-timeout`.
    pub(crate) start_timeout: Duration,
}

impl JobLimits {
    /// The limits of a pool that sets neither: a queue wait of one hour and a start timeout
    /// of fifteen minutes.
    pub(crate) const DEFAULT: JobLimits = JobLimits {
        max_queue_wait: Duration::from_secs(3600),
        start_timeout: Duration::from_secs(900),
    };

    /// The limits the pool's `settings` give.
    pub(crate) fn of(settings: &PoolSettings) -> JobLimits {
        JobLimits {
            max_queue_wait: settings.max_queue_wait,
            start_timeout: settings.start_timeout,
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
/// state, which no limit here ends.
pub(crate) fn deadline(job: &JobRecord, limits: JobLimits) -> Option<(StoreTime, StartFailure)> {
    let (from, limit, failure) = match job.state {
        JobState::Queued => (
            job.submitted_at,
            limits.max_queue_wait,
            StartFailure::NoCapacity,
        ),
        // A placed job always has its placement time; a record without one counts from its
        // submit, the earlier instant.
        JobState::Placed | JobState::Starting => (
            job.placed_at.unwrap_or(job.submitted_at),
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

/// How `job` fails to start at `now` under `limits`, when its deadline is at or before `now`;
/// `None` while it is not out of time, or when no limit here ends it.
pub(crate) fn due(job: &JobRecord, limits: JobLimits, now: StoreTime) -> Option<StartFailure> {
    deadline(job, limits)
        .filter(|(at, _)| now >= *at)
        .map(|(_, failure)| failure)
}

/// Ends each job of `jobs` that is out of time at `now` under `limits`, each in one commit
/// with its allocation's move to releasing, and returns those it ended with how. The jobs read
/// are the queued ones and those of every live allocation (a placed or starting job always has
/// one). A job that moved on between the read and its commit (placed, started, ended, or
/// changed by a heartbeat) is left for the next pass, which reads it again; a job that another
/// writer ended in between keeps that end and is not returned. Each job it ends is told to
/// `metrics` as its commit lands, so a pass that fails later still counts it.
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
    let mut waiting = jobs.queued()?;
    for allocation in jobs.live_allocations()? {
        waiting.extend(jobs.get_job(&allocation.job)?);
    }
    let mut ended = Vec::new();
    for job in waiting {
        let Some(failure) = due(&job, limits, now) else {
            continue;
        };
        match jobs.end_job(&job.id, JobEnd::FailedToStart(failure)) {
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
