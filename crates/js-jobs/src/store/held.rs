//! The requeue after a start a held host refused ([`JobStore::requeue_held`]): a host held for
//! maintenance refuses every start with the code `host-held` and makes nothing, so the job goes
//! back to the queue and its allocation is freed at once, as after a failed whole-host prepare,
//! without counting a failure. One transaction whose preconditions are the versions of the
//! rows it read, so a row that changed in between makes it [`JobStoreError::Stale`].

use std::collections::BTreeMap;

use js_state::{Cond, Store, Txn};
use js_types::{HostId, QueueNote};

use super::index::{put_allocation, put_job};
use super::{Beside, JobStore, ensure, with_rows};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::record::{AllocationRecord, AllocationStep, EndReason, Exclusion, JobRecord, JobState};

/// What the requeue after a start a held host refused writes beside the move, as the caller
/// decided it ([`JobStore::requeue_held`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRequeue {
    /// The hosts the job is kept off from now on, each with until when: its bars with the
    /// refusing host's added.
    pub prepare_bars: BTreeMap<HostId, Exclusion>,
    /// The queue note the job's queue status takes in the same transaction, when there is one.
    pub queue: Option<QueueNote>,
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// A placed or starting job whose start the host of its allocation refused as held goes
    /// back to queued, in one transaction:
    ///
    /// - the job row, at its version, becomes queued with no host and no placement time (its
    ///   submit time is kept, so it keeps its place in the queue), takes `requeue`'s bars and
    ///   queue note, and appends the allocation's host to [`JobRecord::held_refusals`]; its
    ///   failed-prepare count is kept as it is;
    /// - the allocation row, at its version, becomes freed with [`EndReason::Refused`] (the
    ///   host made no container, so there is nothing for it to confirm);
    /// - the rows `beside` gives for the freed allocation are put, as [`JobStore::free_with`]
    ///   puts them;
    /// - the start of the allocation is deleted from the host's dispatch queue, at the version
    ///   read, and no stop is queued: the host already answered the start.
    ///
    /// Its next placement gets a new allocation id. Returns the job and the allocation as
    /// written.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is neither
    /// placed nor starting; [`JobStoreError::IllegalTransition`] when a cancel was asked for (it
    /// is then cancelled, not requeued); [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::AllocationNotBooked`] when the allocation is already releasing or freed
    /// (its first end stands); [`JobStoreError::Stale`] when either row changed before the
    /// commit; as for any read and write.
    pub fn requeue_held(
        &self,
        id: &JobId,
        requeue: HeldRequeue,
        beside: &Beside<'_>,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        ensure(
            matches!(job.state, JobState::Placed | JobState::Starting),
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Placed,
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
        let (allocation, allocation_version) = self.booked_allocation(id)?;
        let now = self.store.now()?;
        let mut held_refusals = job.held_refusals.clone();
        held_refusals.push(allocation.host.clone());
        let mut queued = JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            prepare_bars: requeue.prepare_bars,
            held_refusals,
            ..job.clone()
        };
        queued.queue.apply_all(requeue.queue.as_slice(), now);
        let freed = AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::Refused),
            ..allocation.clone()
        };
        let txn = put_job(Txn::new(), Some(&job), &queued, Cond::Version(job_version))?;
        let txn = put_allocation(
            txn,
            Some(&allocation),
            &freed,
            Cond::Version(allocation_version),
        )?;
        let txn = with_rows(txn, beside(&freed));
        let txn = self.queue_stop(txn, &job, Some(&allocation), false, now)?;
        self.commit(id, txn)?;
        Ok((queued, freed))
    }
}
