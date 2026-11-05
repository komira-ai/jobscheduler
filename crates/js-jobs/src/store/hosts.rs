//! What one host holds, and the end of a job whose host says it is gone, for the server that
//! serves the host's heartbeats (design sections 5.3, 5.7 and 6.2).

use js_state::{Cond, Store, Txn};
use js_types::HostId;

use super::{JobStore, allocation_at, ensure, with_rows};
use crate::codec::{encode_allocation, encode_job};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, EndReason, JobRecord, may_move};
use crate::result::{JobEnd, ended};

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The live (booked and releasing) allocations on `host`, in job id order, each with its
    /// job's record, or `None` for a job whose row was deleted at its retention end (its
    /// allocation is kept as history and may still be releasing). It reads every allocation
    /// row, freed ones included, as [`JobStore::booked_on`] does, and then each live one's job.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when a row cannot be read
    /// (a live allocation that cannot be read is not taken as none);
    /// [`JobStoreError::Store`].
    pub fn live_on(
        &self,
        host: &HostId,
    ) -> Result<Vec<(AllocationRecord, Option<JobRecord>)>, JobStoreError> {
        let mut live = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::ALLOCATION)? {
            let allocation = allocation_at(&key, &row.value)?;
            if allocation.host == *host && allocation.step != AllocationStep::Freed {
                let job = self.read_job(&allocation.job)?.map(|(job, _)| job);
                live.push((allocation, job));
            }
        }
        Ok(live)
    }

    /// Ends the job `id` as `end` says and frees its allocation with `reason`, in one
    /// transaction (design section 5.3, the cancel of a job whose host answers that it never
    /// held it, and section 8, row 38): the job takes its end as [`JobStore::end_job`] writes
    /// it, and its booked or releasing allocation becomes freed as [`JobStore::free`] frees
    /// it, so the job and its allocation never disagree and the host is not left booked. A job
    /// that has already ended keeps its first end and only its allocation is freed; an
    /// allocation already freed keeps its first reason; when both are so, nothing is written.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`], [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::IllegalTransition`] when the job's state cannot end that way;
    /// [`JobStoreError::Stale`] when either row changed before the commit; as for any read and
    /// write.
    pub fn end_and_free(
        &self,
        id: &JobId,
        end: JobEnd,
        reason: EndReason,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        self.end_and_free_with(id, end, reason, &|_| Vec::new())
    }

    /// [`JobStore::end_and_free`], with the rows `beside` gives for the freed allocation
    /// written in the same transaction when this call frees it, each as `(key, value)`
    /// whatever the key holds, as [`JobStore::free_with`] writes them. For an allocation
    /// already freed `beside` is not called.
    ///
    /// # Errors
    /// As [`JobStore::end_and_free`].
    pub fn end_and_free_with(
        &self,
        id: &JobId,
        end: JobEnd,
        reason: EndReason,
        beside: &super::Beside,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        let (allocation, allocation_version) = self.allocation_row(id)?;
        let now = self.store.now();
        let mut txn = Txn::new();
        let job = if job.state.is_ended() {
            job
        } else {
            let to = end.state();
            ensure(
                may_move(job.state, to),
                JobStoreError::IllegalTransition {
                    id: id.clone(),
                    from: job.state,
                    to,
                },
            )?;
            let ended = ended(job, end, now, self.retention);
            txn = txn.put(
                keys::job(id),
                encode_job(&ended)?,
                Cond::Version(job_version),
            );
            ended
        };
        let allocation = if allocation.step == AllocationStep::Freed {
            allocation
        } else {
            let freed = AllocationRecord {
                step: AllocationStep::Freed,
                freed_at: Some(now),
                end_reason: Some(reason),
                ..allocation
            };
            txn = txn.put(
                keys::allocation(id),
                encode_allocation(&freed)?,
                Cond::Version(allocation_version),
            );
            txn = with_rows(txn, beside(&freed));
            freed
        };
        if !txn.ops().is_empty() {
            self.commit(id, txn)?;
        }
        Ok((job, allocation))
    }
}
