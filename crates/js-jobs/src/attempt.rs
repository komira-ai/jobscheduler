//! The attempts of a job (`docs/design/job-retry-and-completion-callback.md`, sections 3.2,
//! 3.5, 3.7 and 3.8): how one ended ([`AttemptRecord`], what [`JobStore::retry_txn`] is told),
//! the attempts that ended as a job's record keeps them ([`Attempt`], in its
//! [`AttemptTrail`]), and where and when one that ran was held ([`AttemptRow`], what
//! [`JobStore::attempt_row`] reads).
//!
//! An attempt is one placement of the job, from that placement until the outcome that ends or
//! retries the job; the requeues before a run (a failed whole-host prepare, a start a host
//! refused as held, for capacity or while it fetches the bundle) stay inside the attempt they
//! happen in. A job's record names its current attempt ([`AttemptTrail::number`]), from 1:
//! the placement after a retry counts the next one, and while the job waits for it the number
//! still names the attempt that ended. Its number, host and times are on the job's record
//! while it is the current attempt; when it ends, in the commit that retries or ends the job,
//! they go into the job's history ([`AttemptTrail::history`], the newest
//! [`MAX_ATTEMPT_HISTORY`], the rest counted in [`AttemptTrail::dropped`]) and into a row of
//! the attempt's own, `attempt/<job id>/<n>`, which keeps its host and times however many
//! attempts follow. An attempt that ends while its job holds no host (a job cancelled, or out
//! of queue time, while it waits) ran nowhere: nothing is appended and no row is written.
//!
//! [`JobStore::retry_txn`]: crate::JobStore::retry_txn
//! [`JobStore::attempt_row`]: crate::JobStore::attempt_row

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_state::StoreTime;
use js_types::{HostId, JobEventKind, QueueCode, QueueNote};

use crate::codec::Carried;
use crate::record::{Exclusion, JobRecord, JobState, StartFailure};
use crate::result::{JobEnd, ResultReason};

/// The most ended attempts a job's record keeps: appending past it drops the oldest, which
/// [`AttemptTrail::dropped`] counts. Each attempt keeps its row ([`AttemptRow`]) whatever the
/// bound drops.
pub const MAX_ATTEMPT_HISTORY: usize = 256;

/// How long [`crate::JobStore::retry_txn`] keeps the job off the host of the attempt it
/// retries, from the retry: a retry follows a fault of the host, so the next attempt tries
/// another host first, and a pool with one capable host can still retry once the bar lapses.
pub const RETRY_HOST_BAR: Duration = Duration::from_secs(15 * 60);

/// `bars` with `host` kept off the job until `until`, the bar of a retry: a bar the host already
/// has that keeps it off at least as long (for life, or until `until` or later) stays, and any
/// other (a back-off, or an earlier end) gives way to it.
pub(crate) fn retry_barred(
    mut bars: BTreeMap<HostId, Exclusion>,
    host: &HostId,
    until: StoreTime,
) -> BTreeMap<HostId, Exclusion> {
    let longer = match bars.get(host) {
        Some(Exclusion::ForLife) => true,
        Some(Exclusion::Until(end)) => *end >= until,
        Some(Exclusion::Backoff(_)) | None => false,
    };
    if !longer {
        bars.insert(host.clone(), Exclusion::Until(until));
    }
    bars
}

/// The note a retry records on the job whose attempt `number`, held by `host`, ended as `end`
/// and whose next attempt may be placed `wait` from the retry: the job waits for its retry
/// ([`QueueCode::WaitingForRetry`]), with a [`JobEventKind::RetryScheduled`] event naming the
/// host, and a detail that says how the attempt ended and the wait in whole seconds, rounded
/// up. A retry is no placement attempt.
pub(crate) fn retry_note(
    host: &HostId,
    end: &AttemptRecord,
    number: u32,
    wait: Duration,
) -> QueueNote {
    let how = match end.outcome {
        AttemptOutcome::HostLost => "its host was lost",
        AttemptOutcome::StartFailed => "it failed to start",
        AttemptOutcome::Succeeded | AttemptOutcome::Failed | AttemptOutcome::Cancelled => {
            "it ended"
        }
    };
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let next = number.saturating_add(1);
    QueueNote {
        code: QueueCode::WaitingForRetry,
        detail: format!(
            "attempt {number} on host {host} ended: {how}; attempt {next} may be placed in {seconds} s"
        ),
        event: Some(JobEventKind::RetryScheduled),
        host: Some(host.clone()),
        attempt: false,
    }
}

/// How one attempt ended, as the job API's `AttemptOutcome` tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The job exited with code 0.
    Succeeded,
    /// The job ran and failed: the reason says how.
    Failed,
    /// The attempt's host was lost.
    HostLost,
    /// The attempt did not get to run: the start failure says why.
    StartFailed,
    /// The client cancelled the job.
    Cancelled,
}

/// How one attempt of a job ended: what the end adds to the attempt the job's record holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// How the attempt ended.
    pub outcome: AttemptOutcome,
    /// The attempt's exit code, when its process exited by itself.
    pub exit_code: Option<i32>,
    /// How the attempt ended, as its result says; `None` for an attempt without a result.
    pub reason: Option<ResultReason>,
    /// Why the attempt failed to start, when it did.
    pub start_failure: Option<StartFailure>,
    /// The agent's own code or text for a start failure, when it gave one.
    pub start_failure_detail: Option<String>,
}

impl AttemptRecord {
    /// How the attempt that `end` ends the job with ended: its outcome by the state `end`
    /// gives the job, its exit code and reason as its result has them, its start failure and
    /// the agent's code of a refused start. What [`crate::JobStore::retry_txn`] is told of an
    /// attempt that retries, as an end of the job records it.
    #[must_use]
    pub fn of_end(end: &JobEnd) -> AttemptRecord {
        let (exit_code, reason) = match end {
            JobEnd::Exited { exit_code, .. } => (Some(*exit_code), Some(ResultReason::Exit)),
            JobEnd::WallLimit { exit_code, .. } => (*exit_code, Some(ResultReason::WallLimit)),
            JobEnd::Memory { exit_code, .. } => (*exit_code, Some(ResultReason::Memory)),
            JobEnd::Cancelled { exit_code, .. } => (*exit_code, Some(ResultReason::Cancelled)),
            JobEnd::InputFailed { exit_code, .. } => (*exit_code, Some(ResultReason::InputFailed)),
            JobEnd::Lost | JobEnd::FailedToStart(_) | JobEnd::Refused { .. } => (None, None),
        };
        let (outcome, start_failure) = match end.state() {
            JobState::Succeeded => (AttemptOutcome::Succeeded, None),
            JobState::Cancelled => (AttemptOutcome::Cancelled, None),
            JobState::Lost => (AttemptOutcome::HostLost, None),
            JobState::FailedToStart(failure) => (AttemptOutcome::StartFailed, Some(failure)),
            _ => (AttemptOutcome::Failed, None),
        };
        let start_failure_detail = match end {
            JobEnd::Refused { detail, .. } => Some(detail.clone()),
            _ => None,
        };
        AttemptRecord {
            outcome,
            exit_code,
            reason,
            start_failure,
            start_failure_detail,
        }
    }

    /// Whether the end was the host's own report (an exit, a stop, a refusal, an input it
    /// could not read), so its host needs no stop: the attempt's other ends (a start that
    /// timed out, a host lost) are the server's, and a stop is queued where the start may
    /// have reached the host, as for a job's end the server decided.
    pub(crate) fn told_by_host(&self) -> bool {
        let refused = matches!(
            self.start_failure,
            Some(StartFailure::AgentRefused | StartFailure::InputUnavailable)
        );
        matches!(
            self.outcome,
            AttemptOutcome::Succeeded | AttemptOutcome::Failed
        ) | refused
    }
}

/// One attempt of a job that ended, as the job's record keeps it in its history: what the job
/// API's `Attempt` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The attempt's number, from 1.
    pub number: u32,
    /// The host the attempt was placed on.
    pub host: HostId,
    /// When the attempt became placeable: the job's submit for the first, the end of the
    /// backoff before it for a later one.
    pub queued_at: StoreTime,
    /// When the attempt was last placed (a requeue before its run places it again).
    pub placed_at: StoreTime,
    /// When the attempt began running; `None` for one that never ran.
    pub started_at: Option<StoreTime>,
    /// When the attempt ended.
    pub ended_at: StoreTime,
    /// How it ended.
    pub end: AttemptRecord,
    /// Whether another attempt was queued after it: true for an attempt
    /// [`crate::JobStore::retry_txn`] ended, even when the job then ended (a cancel, its queue
    /// wait) before that attempt was placed.
    pub retried: bool,
    /// Fields a later build wrote, kept for the next write of the job's row.
    pub(crate) carried: Carried,
}

/// A job's attempts: the number of its current attempt, the attempts that ended (the newest
/// [`MAX_ATTEMPT_HISTORY`], and how many older ones were dropped), when its next attempt may
/// be placed, and every host that held one of them. The default is a job's first attempt
/// with nothing ended, which is also what a row written before these fields reads as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptTrail {
    number: u32,
    history: Vec<Attempt>,
    dropped: u32,
    retry_not_before: Option<StoreTime>,
    hosts: BTreeSet<HostId>,
}

impl Default for AttemptTrail {
    /// The first attempt, nothing ended.
    fn default() -> AttemptTrail {
        AttemptTrail {
            number: 1,
            history: Vec::new(),
            dropped: 0,
            retry_not_before: None,
            hosts: BTreeSet::new(),
        }
    }
}

impl AttemptTrail {
    /// The trail a row holds, its history bound applied.
    pub(crate) fn from_parts(
        number: u32,
        history: Vec<Attempt>,
        dropped: u32,
        retry_not_before: Option<StoreTime>,
        hosts: BTreeSet<HostId>,
    ) -> AttemptTrail {
        let mut trail = AttemptTrail {
            number,
            history,
            dropped,
            retry_not_before,
            hosts,
        };
        trail.bound();
        trail
    }

    /// The current attempt, from 1: the one placed, or about to be, or the last that ended
    /// while the job waits for the next.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The attempts that ended, oldest first: at most the newest [`MAX_ATTEMPT_HISTORY`].
    #[must_use]
    pub fn history(&self) -> &[Attempt] {
        &self.history
    }

    /// How many older attempts were dropped from [`AttemptTrail::history`] to keep its bound.
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// When the job's latest retry made its next attempt placeable, on the store's clock:
    /// set by [`crate::JobStore::retry_txn`] and kept until the job's next retry; `None` for a
    /// job never retried.
    #[must_use]
    pub fn retry_not_before(&self) -> Option<StoreTime> {
        self.retry_not_before
    }

    /// Every host that held one of the job's ended attempts, however many the history bound
    /// dropped: a host among them takes a later attempt only if its agent can tell two
    /// placements of one job apart.
    #[must_use]
    pub fn hosts(&self) -> &BTreeSet<HostId> {
        &self.hosts
    }

    /// How many attempts ended: those kept and those dropped.
    pub(crate) fn ended(&self) -> u64 {
        u64::from(self.dropped) + self.history.len() as u64
    }

    /// The trail of a placement: the next attempt counted when the current one has ended
    /// (the job waits after a retry), and nothing changed for a placement within the current
    /// attempt (the first, or one after a requeue before its run).
    pub(crate) fn placed(mut self) -> AttemptTrail {
        if self.ended() == u64::from(self.number) {
            self.number = self.number.saturating_add(1);
        }
        self
    }

    /// The trail of `job` with its current attempt ended at `ended_at` as `end` says and
    /// appended to the history, its host added to the hosts, with `retried` saying whether
    /// another attempt follows; the trail as it is when the job holds no host, whose attempt
    /// ran nowhere.
    pub(crate) fn closed(
        job: &JobRecord,
        end: AttemptRecord,
        ended_at: StoreTime,
        retried: bool,
    ) -> AttemptTrail {
        let mut trail = job.attempt.clone();
        if let (Some(host), Some(placed_at)) = (&job.host, job.placed_at) {
            trail.hosts.insert(host.clone());
            trail.history.push(Attempt {
                number: trail.number,
                host: host.clone(),
                queued_at: trail.retry_not_before.unwrap_or(job.submitted_at),
                placed_at,
                started_at: job.started_at,
                ended_at,
                end,
                retried,
                carried: Carried::default(),
            });
            trail.bound();
        }
        trail
    }

    /// This trail waiting for its next attempt, placeable from `not_before`.
    pub(crate) fn waiting(self, not_before: StoreTime) -> AttemptTrail {
        AttemptTrail {
            retry_not_before: Some(not_before),
            ..self
        }
    }

    /// Drops the oldest attempts past [`MAX_ATTEMPT_HISTORY`], counting them (saturating).
    fn bound(&mut self) {
        let over = self.history.len().saturating_sub(MAX_ATTEMPT_HISTORY);
        self.history.drain(..over);
        let over = u32::try_from(over).unwrap_or(u32::MAX);
        self.dropped = self.dropped.saturating_add(over);
    }
}

/// Where and when one attempt of a job that ran was held: what a read of an earlier attempt's
/// logs needs of it once the job's history no longer lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRow {
    /// The host the attempt was placed on.
    pub host: HostId,
    /// When the attempt was placed, on the store's clock.
    pub placed_at: StoreTime,
    /// When the attempt ended, on the store's clock.
    pub ended_at: StoreTime,
}

#[cfg(test)]
#[path = "attempt_tests.rs"]
mod tests;
