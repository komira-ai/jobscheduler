//! The attempts of a job (`docs/design/job-retry-and-completion-callback.md`, sections 3.2,
//! 3.7 and 3.8): how one ended ([`AttemptRecord`], what [`JobStore::retry_txn`] is told) and
//! where and when one that ran was held ([`AttemptRow`], what [`JobStore::attempt_row`]
//! reads).
//!
//! An attempt is one placement of the job. Its number, its host and its times are on the
//! job's record while it is the current attempt, so an [`AttemptRecord`] carries only what the
//! end of the attempt adds: its outcome and the codes that say why.
//!
//! [`JobStore::retry_txn`]: crate::JobStore::retry_txn
//! [`JobStore::attempt_row`]: crate::JobStore::attempt_row

use js_state::StoreTime;
use js_types::HostId;

use crate::record::StartFailure;
use crate::result::ResultReason;

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
