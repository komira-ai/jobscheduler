//! Whether an attempt that ended is followed by another
//! (`docs/design/job-retry-and-completion-callback.md`, sections 3.2 to 3.4).
//!
//! An attempt is one placement of a job. Every path that ends a placed, starting or running
//! job for its host's sake asks here first ([`next`]): a result its host listed (a refused
//! start, [`crate::per_job`]), a start that timed out ([`crate::job_expiry`]), and both ways a
//! host is lost ([`crate::job_loss`]). The answer is [`js_types::decide`]'s, given the
//! job's retry policy, how the attempt ended ([`attempt_end`]), the attempt's number and the
//! job's id: only a start failure for a cause of the host and a lost host retry, and only
//! while the policy leaves an attempt after the current one. A job with no policy, one with a
//! cancel asked for (a cancel wins), and one that is not placed, starting or running (a late
//! report of an attempt already retried) has none, and ends as it did before.
//!
//! A retry ([`retry`]) is [`js_jobs::JobStore::retry_txn`]: in one commit the attempt goes into
//! the job's history, its host is barred for 15 minutes, its allocation moves to releasing, and
//! the job is queued again, placeable once the wait [`js_types::decide`] gave has passed
//! from the store's clock now. The job has not ended: it keeps its files, gets no retention end
//! and its callback stays armed. The placement that follows waits for the wait and for the
//! previous allocation to be freed (`js_sched_placer`'s `jobs_retry`).
//!
//! Each retry is counted by how its attempt ended (`js_job_retries_total`), and each end that
//! would have retried with an attempt left (`js_job_attempts_exhausted_total`).

use std::time::Duration;

use js_jobs::{AttemptRecord, JobEnd, JobRecord, JobState, JobStoreError, StartFailure};
use js_types::{AttemptEnd, Decision, RetryPolicy, decide};
use tracing::info;

use crate::job_store::ServerJobs;
use crate::metrics::{MetricsSink, RetryOutcome};

/// What follows an attempt that ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// Another attempt, placeable `wait` after the retry.
    Retry {
        /// The wait before the next attempt may be placed.
        wait: Duration,
    },
    /// The job ends. `exhausted`: the end is one that retries, but the job's retry policy left
    /// no attempt after this one.
    End {
        /// Whether the policy's attempts ran out.
        exhausted: bool,
    },
}

/// How an attempt that ended as `end` ended, as the retry decision classes it: an agent's
/// refusal by its code, the part of the job's detail before its first `:` (a refusal that sent
/// a job back too often names its hosts after the code); a refusal without a code is the
/// host's, as a code this server does not know is.
pub(crate) fn attempt_end(end: &JobEnd) -> AttemptEnd {
    match end {
        JobEnd::Exited { exit_code: 0, .. } => AttemptEnd::Succeeded,
        JobEnd::Exited { .. } => AttemptEnd::Exited,
        JobEnd::WallLimit { .. } => AttemptEnd::WallLimit,
        JobEnd::Memory { .. } => AttemptEnd::Memory,
        JobEnd::InputFailed { .. } => AttemptEnd::InputFailed,
        JobEnd::Cancelled { .. } => AttemptEnd::Cancelled,
        JobEnd::Lost => AttemptEnd::HostLost,
        JobEnd::FailedToStart(failure) => start_failed(*failure, ""),
        JobEnd::Refused { failure, detail } => {
            let code = detail.split(':').next().unwrap_or_default();
            start_failed(*failure, code)
        }
    }
}

/// How a start that failed for `failure`, refused with the agent's `code` (empty for none),
/// ended.
fn start_failed(failure: StartFailure, code: &str) -> AttemptEnd {
    match failure {
        StartFailure::NoCapacity => AttemptEnd::NoCapacity,
        StartFailure::StartTimeout => AttemptEnd::StartTimeout,
        StartFailure::ExclusiveNotReady => AttemptEnd::ExclusiveNotReady,
        StartFailure::InputUnavailable => AttemptEnd::InputUnavailable,
        StartFailure::AgentRefused => AttemptEnd::AgentRefused {
            code: code.to_owned(),
        },
    }
}

/// What follows the current attempt of `job` when it ends as `end` (see the module docs):
/// [`Next::Retry`] with the wait when the job is placed, starting or running with no cancel
/// asked for, its policy retries the end and leaves an attempt after this one; [`Next::End`]
/// otherwise, `exhausted` when only the attempts ran out.
pub(crate) fn next(job: &JobRecord, end: &JobEnd) -> Next {
    let live = matches!(
        job.state,
        JobState::Placed | JobState::Starting | JobState::Running
    );
    let policy = job.spec.retry.filter(|_| live & !job.cancel_requested);
    let Some(policy) = policy else {
        return Next::End { exhausted: false };
    };
    let (end, number) = (attempt_end(end), job.attempt.number());
    match decide(&policy, &end, number, &job.id) {
        Decision::Retry { wait } => Next::Retry { wait },
        Decision::End => {
            // The same end under a policy with every attempt left: whether the end retries.
            let more = RetryPolicy {
                max_attempts: u32::MAX,
                ..policy
            };
            let retries = decide(&more, &end, number, &job.id) != Decision::End;
            Next::End { exhausted: retries }
        }
    }
}

/// Queues the next attempt of `job`, whose current attempt ended as `end`, placeable `wait`
/// from the store's clock now ([`js_jobs::JobStore::retry_txn`]), counts the retry by how the
/// attempt ended and answers the job as queued.
///
/// # Errors
/// As [`js_jobs::JobStore::retry_txn`]: [`JobStoreError::IllegalTransition`] when the job moved
/// on (it ended, or a cancel was asked for) since it was read, [`JobStoreError::Stale`] when it
/// changed before the commit; nothing is written or counted.
pub(crate) fn retry(
    jobs: &ServerJobs,
    job: &JobRecord,
    end: &JobEnd,
    wait: Duration,
    metrics: &dyn MetricsSink,
) -> Result<JobRecord, JobStoreError> {
    let not_before = jobs.now()?.saturating_add(wait);
    let queued = jobs.retry_txn(&job.id, AttemptRecord::of_end(end), not_before)?;
    let outcome = match end {
        JobEnd::Lost => RetryOutcome::HostLost,
        _ => RetryOutcome::StartFailed,
    };
    metrics.job_retried(outcome);
    let attempt = job.attempt.number();
    info!(job = %job.id, attempt, ?outcome, ?wait, "an attempt ended in a way that retries; the job's next attempt is queued");
    Ok(queued)
}

/// What [`end_or_retry`] made of an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The job ended: the record [`js_jobs::JobStore::end_job`] answered (a job another writer
    /// ended first keeps that end).
    Ended(JobRecord),
    /// The job's next attempt was queued.
    Retried(JobRecord),
}

/// Ends the current attempt of `job` as `end`: a retry when [`next`] says one follows
/// ([`retry`]), else the job's end ([`js_jobs::JobStore::end_job`], in one commit with its
/// allocation's move to releasing), an end whose policy's attempts ran out counted when it is
/// the one the commit made.
///
/// # Errors
/// As [`retry`] or [`js_jobs::JobStore::end_job`].
pub(crate) fn end_or_retry(
    jobs: &ServerJobs,
    job: &JobRecord,
    end: JobEnd,
    metrics: &dyn MetricsSink,
) -> Result<Settled, JobStoreError> {
    match next(job, &end) {
        Next::Retry { wait } => retry(jobs, job, &end, wait, metrics).map(Settled::Retried),
        Next::End { exhausted } => {
            let state = end.state();
            let ended = jobs.end_job(&job.id, end)?;
            if exhausted & (ended.state == state) {
                metrics.attempts_exhausted();
            }
            Ok(Settled::Ended(ended))
        }
    }
}

#[cfg(test)]
#[path = "job_retry_tests.rs"]
mod tests;
