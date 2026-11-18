//! The requeue of a running job whose mount provider lost its tree
//! ([`JobStore::requeue_lost_tree`]): the one way a job goes from running back to queued.

use js_state::{Cond, Store, Txn};
use js_types::QueueNote;

use super::index::{put_allocation, put_job};
use super::{JobStore, ensure};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::record::{AllocationRecord, AllocationStep, JobRecord, JobState};

/// How many times a job is queued again because a mount provider lost its tree: a job runs at
/// most three times for lost trees, and the third loss ends it as an input failure.
pub const TREE_LOST_REQUEUES: u32 = 2;

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// A running job whose mount provider lost its tree goes back to queued, in one
    /// transaction: no host, no placement time, [`JobRecord::lost_trees`] one more,
    /// and `note` applied to its queue status at the store's time now. Its start time stays
    /// until its next start replaces it, so the usage history of the run that lost its tree
    /// (written when its allocation is freed) counts that run. Its booked allocation
    /// moves to releasing, as at every end of a job: the container has exited, and the host
    /// still removes it and confirms, and the next placement waits for that free and gets a new
    /// allocation id. The count is not checked against [`TREE_LOST_REQUEUES`]; the caller
    /// decides whether the job is queued again or ended.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is not
    /// running; [`JobStoreError::IllegalTransition`] when a cancel was asked for (the job is
    /// then cancelled, not requeued); [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::AllocationNotBooked`] when the allocation is already releasing or
    /// freed; [`JobStoreError::Stale`]; as for any read and write.
    pub fn requeue_lost_tree(
        &self,
        id: &JobId,
        note: QueueNote,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        ensure(
            job.state == JobState::Running,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Running,
                found: job.state,
            },
        )?;
        ensure(
            !job.cancel_requested,
            JobStoreError::IllegalTransition {
                id: id.clone(),
                from: job.state,
                to: JobState::Queued,
            },
        )?;
        let (allocation, allocation_version) = self.allocation_row(id)?;
        ensure(
            allocation.step == AllocationStep::Booked,
            JobStoreError::AllocationNotBooked {
                id: id.clone(),
                step: allocation.step,
            },
        )?;
        let now = self.store.now();
        let mut queued = JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            lost_trees: job.lost_trees.saturating_add(1),
            ..job.clone()
        };
        queued.queue.apply_all(&[note], now);
        let releasing = AllocationRecord {
            step: AllocationStep::Releasing,
            ..allocation.clone()
        };
        let txn = put_job(Txn::new(), Some(&job), &queued, Cond::Version(job_version))?;
        let txn = put_allocation(
            txn,
            Some(&allocation),
            &releasing,
            Cond::Version(allocation_version),
        )?;
        self.commit(id, txn)?;
        Ok((queued, releasing))
    }
}
