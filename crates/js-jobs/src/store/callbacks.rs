//! The delivery of jobs' completion callbacks (`docs/design/job-retry-and-completion-callback.md`,
//! section 5.5): the claim of due calls ([`JobStore::claim_callbacks`]), the answer of one
//! ([`JobStore::answer_callback`]) and where a job's delivery stands
//! ([`JobStore::callback_status`]).
//!
//! This release keeps no callback rows: no job has a callback, so none is ever due, and an
//! answer finds no claim to answer.

use js_state::{Store, StoreTime};
use js_types::CallbackAnswer;

use super::JobStore;
use crate::callback::{CallbackClaim, CallbackStatus};
use crate::error::JobStoreError;
use crate::ids::{IdSource, InstanceId, JobId};

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Claims up to `limit` callbacks due at `now` for the instance `by`, each until a claim
    /// lapses, so no other instance sends it meanwhile.
    ///
    /// This release keeps no callback rows, so it claims none and writes nothing.
    ///
    /// # Errors
    /// A store error when the due callbacks cannot be read or claimed.
    pub fn claim_callbacks(
        &self,
        by: &InstanceId,
        now: StoreTime,
        limit: usize,
    ) -> Result<Vec<CallbackClaim>, JobStoreError> {
        let _ = (by, now, limit);
        Ok(Vec::new())
    }

    /// Records `answer` to the try `claim` made at `now`, at the version the claim holds: a
    /// delivery, an abandon, or a failure with the wait before the next try.
    ///
    /// This release keeps no callback rows, so no row holds the claim: the answer is refused
    /// as stale and writes nothing.
    ///
    /// # Errors
    /// [`JobStoreError::Stale`] when the callback's row no longer holds `claim`; a store error
    /// when the answer cannot be written.
    pub fn answer_callback(
        &self,
        claim: CallbackClaim,
        answer: CallbackAnswer,
        now: StoreTime,
    ) -> Result<(), JobStoreError> {
        let _ = (answer, now);
        Err(JobStoreError::Stale { id: claim.job })
    }

    /// Where the delivery of the callback of the job `id` stands. `None` when the job has no
    /// callback.
    ///
    /// This release keeps no callback rows, so every job that exists is `None`.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`; a store error when the job's row
    /// cannot be read.
    pub fn callback_status(&self, id: &JobId) -> Result<Option<CallbackStatus>, JobStoreError> {
        self.job_row(id)?;
        Ok(None)
    }
}
