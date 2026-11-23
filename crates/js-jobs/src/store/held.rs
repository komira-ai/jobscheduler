//! The requeue after a start a host refused ([`JobStore::requeue_held`]): a host held for
//! maintenance refuses every start with the code `host-held`, a host with no room for the job
//! refuses it with the code `no-capacity`, and a host still fetching the job's bundle refuses
//! it with the code `bundle-pending`; each makes nothing, so the job goes back to the queue and its allocation is freed at once, as after a failed whole-host prepare, without
//! counting a failure. One transaction whose preconditions are the versions of the
//! rows it read, so a row that changed in between makes it [`JobStoreError::Stale`].

use std::collections::BTreeMap;

use js_state::{Cond, Store, Txn};
use js_types::{HostId, QueueNote};

use super::index::{put_allocation, put_job};
use super::{Beside, JobStore, ensure, with_rows};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::record::{AllocationRecord, AllocationStep, EndReason, Exclusion, JobRecord, JobState};

/// Which refusal sent a job back to the queue ([`HeldRequeue::refused`]): the list of the job
/// row that the refusing host is appended to, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeuedFor {
    /// The host is held for maintenance (the agent's code `host-held`): the host is appended to
    /// [`JobRecord::held_refusals`].
    Held,
    /// The host had no room for the job (the agent's code `no-capacity`): the host is appended
    /// to [`JobRecord::capacity_refusals`].
    Capacity,
    /// The host is fetching the job's bundle and holds it not yet (the agent's code
    /// `bundle-pending`): the host is appended to no list. The fetch ends on its own, held or
    /// failed, so nothing counts these requeues; the job's queue deadline bounds the wait.
    BundlePending,
}

/// What the requeue after a start a host refused writes beside the move, as the caller decided
/// it ([`JobStore::requeue_held`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRequeue {
    /// The refusal that sent the job back, which decides the list the refusing host is added to,
    /// if any.
    pub refused: RequeuedFor,
    /// The hosts the job is kept off from now on, each with until when: its bars with the
    /// refusing host's added.
    pub prepare_bars: BTreeMap<HostId, Exclusion>,
    /// The queue note the job's queue status takes in the same transaction, when there is one.
    pub queue: Option<QueueNote>,
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// A placed or starting job whose start the host of its allocation refused, as held, for
    /// capacity or while it fetches the job's bundle, goes back to queued, in one transaction:
    ///
    /// - the job row, at its version, becomes queued with no host and no placement time (its
    ///   submit time is kept, so it keeps its place in the queue), takes `requeue`'s bars and
    ///   queue note, and appends the allocation's host to [`JobRecord::held_refusals`] when
    ///   `requeue` is for [`RequeuedFor::Held`] and to [`JobRecord::capacity_refusals`] when
    ///   it is for [`RequeuedFor::Capacity`], and to neither when it is for
    ///   [`RequeuedFor::BundlePending`]; its failed-prepare count is kept as it is;
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
        let mut capacity_refusals = job.capacity_refusals.clone();
        match requeue.refused {
            RequeuedFor::Held => held_refusals.push(allocation.host.clone()),
            RequeuedFor::Capacity => capacity_refusals.push(allocation.host.clone()),
            RequeuedFor::BundlePending => {}
        }
        let mut queued = JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            prepare_bars: requeue.prepare_bars,
            held_refusals,
            capacity_refusals,
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
