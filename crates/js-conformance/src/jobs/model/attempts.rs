//! The attempts and the completion callback of the reference service: how an attempt ends,
//! whether another follows, what the job shows between two attempts, and when its callback
//! is due and delivered (`docs/design/job-retry-and-completion-callback.md`, sections 3 and
//! 5). The decision is [`js_types::decide`], given the job's policy, the job's id and the
//! attempt's number, so the wait is the real service's.

use std::time::Duration;

use js_proto::v1::{self, JobState, ResultReason};
use js_types::{AttemptEnd, Decision, JobId, RetryPolicy, decide};

use super::{Defect, Inner, Refused, Stored, duration_millis};

/// How an attempt of a job ends, for the moves that can retry it.
#[derive(Debug, Clone, Copy)]
pub(super) enum Kind {
    /// The job's host is lost.
    HostLost,
    /// The job's start times out.
    StartFailed,
    /// The job's process exits with this nonzero code: the end the service never retries,
    /// which [`Defect::ExitRetries`] retries.
    Exited(i32),
}

impl Kind {
    /// The state the job ends in when this ends the attempt and no other follows.
    fn ended_as(self) -> JobState {
        match self {
            Kind::HostLost => JobState::Lost,
            Kind::StartFailed => JobState::FailedToStart,
            Kind::Exited(_) => JobState::Failed,
        }
    }

    /// The exit code of the attempt, when its process exited.
    fn code(self) -> Option<i32> {
        match self {
            Kind::Exited(code) => Some(code),
            Kind::HostLost | Kind::StartFailed => None,
        }
    }

    /// The reason of the attempt's result: none for an attempt that did not exit.
    fn reason(self) -> ResultReason {
        match self {
            Kind::Exited(_) => ResultReason::Exit,
            Kind::HostLost | Kind::StartFailed => ResultReason::Unspecified,
        }
    }

    /// How the attempt ended, as the retry decision classes it. An exit is classed as a lost
    /// host: only [`Defect::ExitRetries`] asks.
    fn class(self) -> AttemptEnd {
        match self {
            Kind::StartFailed => AttemptEnd::StartTimeout,
            Kind::HostLost | Kind::Exited(_) => AttemptEnd::HostLost,
        }
    }

    /// The words the job's queue status says the attempt ended in.
    fn how(self) -> &'static str {
        match self {
            Kind::HostLost => "its host was lost",
            Kind::StartFailed => "it failed to start",
            Kind::Exited(_) => "it ended",
        }
    }

    /// Whether a job in `state` is in a state this can end an attempt from.
    fn may_end(self, state: JobState) -> bool {
        match self {
            Kind::HostLost => matches!(
                state,
                JobState::Placed | JobState::Starting | JobState::Running
            ),
            Kind::StartFailed => matches!(state, JobState::Placed | JobState::Starting),
            Kind::Exited(_) => state == JobState::Running,
        }
    }
}

/// How long the callback of a job that names no deadline is tried after the job's end.
const DEFAULT_DEADLINE_MS: u64 = 86_400_000;

/// The wait before the next attempt when `kind` ends the current attempt of `stored` and the
/// job retries: its policy leaves an attempt, `off` does not switch this kind's retry off, and
/// no cancel was asked for (or `cancel_retries` ignores it). `None`: the job ends.
fn retry_wait(stored: &Stored, kind: Kind, off: bool, cancel_retries: bool) -> Option<Duration> {
    let asked = stored.job.cancel_requested & !cancel_retries;
    let given = stored.key.retry.as_ref().filter(|_| !off & !asked)?;
    let seconds = |seconds: u32| Duration::from_secs(seconds.into());
    let policy = RetryPolicy {
        max_attempts: given.max_attempts,
        initial_backoff: given.initial_backoff_seconds.map(seconds),
        max_backoff: given.max_backoff_seconds.map(seconds),
    };
    let number = stored.job.attempt;
    let decision = JobId::parse(&stored.job.id).map_or(Decision::End, |id| {
        decide(&policy, &kind.class(), number, &id)
    });
    match decision {
        Decision::Retry { wait } => Some(wait),
        Decision::End => None,
    }
}

impl Stored {
    /// Puts the attempt that ended in `ended_as` at `now` into the job's history, as
    /// `retried` says; nothing when the job holds no host (a queued job that is cancelled
    /// ran nowhere).
    pub(super) fn close(
        &mut self,
        ended_as: JobState,
        exit_code: Option<i32>,
        reason: ResultReason,
        retried: bool,
        now: u64,
    ) {
        if self.job.host_id.is_empty() {
            return;
        }
        let (outcome, start_failure) = match ended_as {
            JobState::Succeeded => (v1::AttemptOutcome::Succeeded, None),
            JobState::Cancelled => (v1::AttemptOutcome::Cancelled, None),
            JobState::Lost => (v1::AttemptOutcome::HostLost, None),
            JobState::FailedToStart => (
                v1::AttemptOutcome::StartFailed,
                Some(v1::StartFailureCode::StartTimeout),
            ),
            _ => (v1::AttemptOutcome::Failed, None),
        };
        let held = self.job.times.unwrap_or_default();
        let queued_at_ms = self.retry_not_before.unwrap_or(held.queued_at_ms);
        self.job.attempt_history.push(v1::Attempt {
            number: self.job.attempt,
            host_id: self.job.host_id.clone(),
            outcome: outcome.into(),
            times: Some(v1::JobTimes {
                queued_at_ms,
                ended_at_ms: Some(now),
                ..held
            }),
            exit_code,
            reason: reason.into(),
            start_failure: start_failure.unwrap_or_default().into(),
            start_failure_detail: String::new(),
            retried,
        });
    }

    /// Ends the current attempt as `kind` says and queues the next, placeable `wait` from
    /// now: the attempt goes into the history as retried, the job loses its host and its
    /// placed and started times, waits for its retry, and has not ended, so it has no result
    /// and no retention end. `per_attempt` makes its callback due, which no job does.
    fn retry(&mut self, kind: Kind, wait: Duration, clock: (u64, u64), per_attempt: bool) {
        let (now, retention_ms) = clock;
        self.close(kind.ended_as(), kind.code(), kind.reason(), true, now);
        let until = now.saturating_add(duration_millis(wait));
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let (number, host) = (self.job.attempt, self.job.host_id.clone());
        let how = kind.how();
        self.job.host_id.clear();
        let mut times = self.job.times.unwrap_or_default();
        times.placed_at_ms = None;
        times.started_at_ms = None;
        self.job.times = Some(times);
        self.state = JobState::Queued;
        self.job.state = JobState::Queued.into();
        self.retry_not_before = Some(until);
        self.job.retry_not_before_ms = Some(until);
        self.job.queue_status = Some(v1::QueueStatus {
            code: v1::QueueStatusCode::WaitingForRetry.into(),
            detail: format!(
                "attempt {number} on host {host} ended: {how}; attempt {} may be placed in {seconds} s",
                number + 1
            ),
            since_ms: now,
        });
        if per_attempt {
            self.due_callback(now, now.saturating_add(retention_ms));
        }
    }

    /// Makes the callback of the job due at `now` when it is armed: pending, tried until its
    /// deadline from now (a day when it names none), cut to `retention_ends`.
    pub(super) fn due_callback(&mut self, now: u64, retention_ends: u64) {
        let deadline_ms = self
            .key
            .callback
            .as_ref()
            .and_then(|callback| callback.deadline_seconds)
            .map_or(DEFAULT_DEADLINE_MS, |seconds| u64::from(seconds) * 1000);
        let armed = |status: &&mut v1::CallbackStatus| status.state() == v1::CallbackState::Armed;
        if let Some(status) = self.job.callback_status.as_mut().filter(armed) {
            status.set_state(v1::CallbackState::Pending);
            status.deadline_at_ms = Some(now.saturating_add(deadline_ms).min(retention_ends));
        }
    }
}

impl Inner {
    /// Ends the current attempt of job `job` as `kind` says: when the job retries it is
    /// queued for its next attempt and `true` is answered; when it does not, a lost host
    /// ends it lost (keeping `cancel_requested`) and a start that failed ends it failed to
    /// start, and `false` is answered. An exit that does not retry is left to the caller,
    /// which ends the job as any exit does, and answers `false` too.
    ///
    /// # Errors
    /// [`Refused`] when no job has the id, or it is in no state `kind` ends an attempt from.
    pub(super) fn attempt_ended(&mut self, job: &str, kind: Kind) -> Result<bool, Refused> {
        let off = match kind {
            Kind::HostLost => self.has(Defect::NoRetryOnHostLoss),
            Kind::StartFailed => self.has(Defect::NoRetryOnStartFailure),
            Kind::Exited(_) => !self.has(Defect::ExitRetries),
        };
        let cancel_retries = self.has(Defect::CancelRetries);
        let per_attempt = self.has(Defect::CallbackPerAttempt);
        let clock = self.clock();
        let stored = self.jobs.get_mut(job).ok_or(Refused)?;
        if !kind.may_end(stored.state) {
            return Err(Refused);
        }
        if let Some(wait) = retry_wait(stored, kind, off, cancel_retries) {
            stored.retry(kind, wait, clock, per_attempt);
            return Ok(true);
        }
        if let Kind::Exited(_) = kind {
            return Ok(false);
        }
        let (state, asked) = (kind.ended_as(), stored.job.cancel_requested);
        stored.end(state, None, ResultReason::Unspecified, clock);
        stored.job.cancel_requested = asked & (state == JobState::Lost);
        Ok(false)
    }

    /// One delivery try of the callback of job `job`, answered 2xx: a callback that is
    /// pending is delivered, with one more try.
    ///
    /// # Errors
    /// [`Refused`] when no job has the id, or its callback is not pending: the job has none,
    /// has not ended, or delivered it already.
    pub(super) fn deliver_callback(&mut self, job: &str) -> Result<(), Refused> {
        let now = self.now_ms;
        let stored = self.jobs.get_mut(job).ok_or(Refused)?;
        let pending =
            |status: &&mut v1::CallbackStatus| status.state() == v1::CallbackState::Pending;
        let due = stored.job.callback_status.as_mut().filter(pending);
        let status = due.ok_or(Refused)?;
        status.set_state(v1::CallbackState::Delivered);
        status.tries += 1;
        status.last_try_at_ms = Some(now);
        status.delivered_at_ms = Some(now);
        Ok(())
    }
}
