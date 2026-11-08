//! The queue status of a queued job (`docs/design/queue-status.md`, section 5): the notes the
//! placement engine decided for it, applied in one transaction ([`JobStore::note_queue`]).
//! A placement and a requeue apply theirs in their own transactions
//! ([`super::Placement::queue`], [`super::FailedPrepare::queue`]).

use js_state::{Cond, Store, Txn};
use js_types::QueueNote;

use super::JobStore;
use super::index::put_job;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::record::{JobRecord, JobState};

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Applies `notes`, in order, to the queue status of the queued job `id` at the store's
    /// time now ([`crate::QueueTrail::apply`]), in one transaction at the version read. Writes
    /// nothing, and answers `None`, when the job is not queued (it was placed, cancelled or
    /// ended in between) or the notes change nothing; otherwise the job as written.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::Stale`] when the row changed before the
    /// commit; as for any read and write.
    pub fn note_queue(
        &self,
        id: &JobId,
        notes: &[QueueNote],
    ) -> Result<Option<JobRecord>, JobStoreError> {
        let (job, version) = self.job_row(id)?;
        if job.state != JobState::Queued {
            return Ok(None);
        }
        let now = self.store.now();
        let mut noted = job.clone();
        if !noted.queue.apply_all(notes, now) {
            return Ok(None);
        }
        let txn = put_job(Txn::new(), Some(&job), &noted, Cond::Version(version))?;
        self.commit(id, txn)?;
        Ok(Some(noted))
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
