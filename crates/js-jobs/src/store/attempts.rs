//! A job's next attempt ([`JobStore::retry_txn`]) and the row of an attempt that ran
//! ([`JobStore::attempt_row`]) (`docs/design/job-retry-and-completion-callback.md`, sections
//! 3.7 and 3.8).
//!
//! This release keeps no attempt rows and makes no retry: [`JobStore::retry_txn`] refuses the
//! move back to queued as the illegal move it is today, and writes nothing, and
//! [`JobStore::attempt_row`] finds no row for any job.

use js_state::{Store, StoreTime};

use super::JobStore;
use crate::attempt::{AttemptRecord, AttemptRow};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::record::{JobRecord, JobState};

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Ends the current attempt of the job `id` as `end` says and queues its next attempt,
    /// placeable from `not_before`, in one commit beside [`JobStore::end_job`]'s, and answers
    /// the job as queued.
    ///
    /// This release makes no retry: it reads the job and refuses the move, whatever the job's
    /// state, writing nothing.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`;
    /// [`JobStoreError::IllegalTransition`] from the job's state to queued otherwise; a store
    /// error when the job's row cannot be read.
    pub fn retry_txn(
        &self,
        id: &JobId,
        end: AttemptRecord,
        not_before: StoreTime,
    ) -> Result<JobRecord, JobStoreError> {
        let (job, _) = self.job_row(id)?;
        let _ = (end, not_before);
        Err(JobStoreError::IllegalTransition {
            id: id.clone(),
            from: job.state,
            to: JobState::Queued,
        })
    }

    /// The row of attempt `attempt` (counted from 1) of the job `id`: its host and when it
    /// was placed and ended. `None` for an attempt that never ran.
    ///
    /// This release keeps no attempt rows, so every attempt of a job that exists is `None`.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`] when there is no job `id`; a store error when the job's row
    /// cannot be read.
    pub fn attempt_row(
        &self,
        id: &JobId,
        attempt: u32,
    ) -> Result<Option<AttemptRow>, JobStoreError> {
        self.job_row(id)?;
        let _ = attempt;
        Ok(None)
    }
}
