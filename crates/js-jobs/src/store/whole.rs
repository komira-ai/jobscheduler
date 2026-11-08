//! The operations a whole-host job needs beyond the ones every job has (design sections 5.4
//! and 6.2): a placement that writes its prepare deadline and its own wall limit and drops the
//! job's mark in the same commit ([`JobStore::place_with`]), the failed prepare as the placement
//! engine decided it ([`JobStore::fail_prepare_with`]), and the restart of a prepare after a
//! hello ([`JobStore::restart_prepare`]). Each is one transaction whose preconditions are the
//! versions of the rows it read, so a row that changed in between makes it
//! [`JobStoreError::Stale`].
//!
//! The reads and refusals that a placement, a requeue and a restart share are here too, so the
//! operations in `store.rs` and in this file refuse the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_state::{Cond, Store, StoreTime, Txn, Version};
use js_types::{HostId, QueueNote};

use super::index::{put_allocation, put_job};
use super::{Beside, JobStore, ensure, with_rows};
use crate::codec::Carried;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{
    AllocationRecord, AllocationStep, Booked, EndReason, Exclusion, JobRecord, JobState, Needs,
    StartFailure,
};
use crate::result::{JobEnd, ended};

/// What a placement writes beside its booking ([`JobStore::place_with`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// For a whole-host placement: how long the agent has to report the host ready; the
    /// allocation's [`AllocationRecord::prepare_deadline`] is the placement's time plus this.
    /// `None`: no deadline.
    pub prepare_timeout: Option<Duration>,
    /// The placement's wall limit when it is shorter than the job's own
    /// ([`AllocationRecord::wall_limit_secs`]).
    pub wall_limit_secs: Option<u64>,
    /// The queue notes the engine decided for the job, applied in order to its queue status
    /// in the same transaction (`docs/design/queue-status.md`): what pass 2 told it since the
    /// last pass, then its placement.
    pub queue: Vec<QueueNote>,
}

/// How a failed whole-host prepare ends, as the placement engine decided it
/// ([`JobStore::fail_prepare_with`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedPrepare {
    /// The job's failed prepares, this one counted.
    pub failed_prepares: u32,
    /// The hosts the job is kept off from now on, each with until when.
    pub prepare_bars: BTreeMap<HostId, Exclusion>,
    /// Whether the job has used up its prepares: it ends failed to start with
    /// [`StartFailure::ExclusiveNotReady`] instead of going back to queued.
    pub terminal: bool,
    /// The queue note the engine decided for a job that goes back to queued, applied to its
    /// queue status in the same transaction when there is one; the engine gives none for a
    /// terminal failure, whose job keeps the status of its placement.
    pub queue: Option<QueueNote>,
}

/// A job and its allocation, each with the version it was read at.
type Rows = ((JobRecord, Version), (AllocationRecord, Version));

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// [`JobStore::place`], with what `placement` says written beside the booking and the job's
    /// mark dropped in the same transaction. The allocation row gets a
    /// [`AllocationRecord::prepare_deadline`] of the store's time now plus
    /// [`Placement::prepare_timeout`] (the time that is also the job's `placed_at`), and the
    /// placement's [`AllocationRecord::wall_limit_secs`]. The job's `mark/<id>` is deleted at
    /// the version read, or its absence is a precondition when there is none, so a mark written
    /// in between makes the placement stale rather than leaving a mark on a placed job.
    ///
    /// # Errors
    /// As [`JobStore::place`]; [`JobStoreError::Stale`] also when the mark row changed (or was
    /// written) before the commit.
    pub fn place_with(
        &self,
        id: &JobId,
        host: HostId,
        booked: Booked,
        placement: Placement,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let (txn, placed, allocation) = self.placement_txn(id, host, booked, placement)?;
        let txn = self.without_mark(txn, id)?;
        self.commit(id, txn)?;
        Ok((placed, allocation))
    }

    /// The transaction that places the queued job `id` on `host` as [`JobStore::place`]
    /// describes, with `placement` written beside the booking, and the job and allocation it
    /// writes.
    pub(super) fn placement_txn(
        &self,
        id: &JobId,
        host: HostId,
        booked: Booked,
        placement: Placement,
    ) -> Result<(Txn, JobRecord, AllocationRecord), JobStoreError> {
        let (job, job_version) = self.job_row(id)?;
        ensure(
            job.state == JobState::Queued,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Queued,
                found: job.state,
            },
        )?;
        let earlier = self.read_allocation(id)?;
        let (allocation_cond, earlier_id) = match &earlier {
            None => (Cond::Absent, None),
            Some((earlier, version)) => {
                ensure(
                    earlier.step == AllocationStep::Freed,
                    JobStoreError::AllocationHeld {
                        id: id.clone(),
                        step: earlier.step,
                    },
                )?;
                (Cond::Version(*version), Some(earlier.allocation_id))
            }
        };
        let allocation_id = self.fresh_allocation_id(earlier_id)?;
        let now = self.store.now();
        let host_ports: BTreeSet<u16> = job.spec.ports.iter().map(|port| port.host).collect();
        let allocation = AllocationRecord {
            job: id.clone(),
            allocation_id,
            host: host.clone(),
            booked,
            host_ports: host_ports.into_iter().collect(),
            step: AllocationStep::Booked,
            created_at: now,
            freed_at: None,
            end_reason: None,
            prepare_deadline: placement
                .prepare_timeout
                .map(|timeout| now.saturating_add(timeout)),
            wall_limit_secs: placement.wall_limit_secs,
            carried: Carried::default(),
        };
        let mut placed = JobRecord {
            state: JobState::Placed,
            host: Some(host),
            placed_at: Some(now),
            ..job.clone()
        };
        placed.queue.apply_all(&placement.queue, now);
        let txn = put_job(Txn::new(), Some(&job), &placed, Cond::Version(job_version))?;
        let earlier = earlier.as_ref().map(|(record, _)| record);
        let txn = put_allocation(txn, earlier, &allocation, allocation_cond)?;
        let txn = super::taints::with_taint(txn, &placed, &allocation.host)?;
        Ok((txn, placed, allocation))
    }

    /// The failed prepare of a placed whole-host job, as the placement engine decided it
    /// (design sections 5.4 and 6.2; the one allocation freed with no confirmation from the
    /// host, since no container was made). In one transaction:
    ///
    /// - the job row, at its version, takes `failed`'s count and bars; when `failed` is not
    ///   terminal it goes back to queued with no host and no placement time (its submit time
    ///   is kept, so it keeps its place in the queue), and when it is terminal it ends failed to
    ///   start with [`StartFailure::ExclusiveNotReady`], as [`JobStore::end_job`] ends a job;
    /// - the allocation row, at its version, becomes freed with [`EndReason::FailedPrepare`];
    /// - the rows `beside` gives for the freed allocation are put, as [`JobStore::free_with`]
    ///   puts them;
    /// - the job's `mark/<id>` is deleted at the version read, or its absence is a
    ///   precondition when there is none.
    ///
    /// Returns the job and the allocation as written.
    ///
    /// # Errors
    /// As [`JobStore::requeue_failed_prepare`]: [`JobStoreError::UnexpectedState`] when the job
    /// is not placed; [`JobStoreError::IllegalTransition`] when it does not need the whole host
    /// or a cancel was asked for; [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::AllocationNotBooked`]; [`JobStoreError::Stale`], also when the mark row
    /// changed (or was written) before the commit; as for any read and write.
    pub fn fail_prepare_with(
        &self,
        id: &JobId,
        failed: FailedPrepare,
        beside: &Beside<'_>,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let ((job, job_version), (allocation, allocation_version)) = self.prepare_rows(id)?;
        let now = self.store.now();
        let FailedPrepare {
            failed_prepares,
            prepare_bars,
            terminal,
            queue,
        } = failed;
        let mut counted = JobRecord {
            failed_prepares,
            prepare_bars,
            ..job.clone()
        };
        counted.queue.apply_all(queue.as_slice(), now);
        let after = if terminal {
            let end = JobEnd::FailedToStart(StartFailure::ExclusiveNotReady);
            ended(counted, end, now, self.retention)
        } else {
            JobRecord {
                state: JobState::Queued,
                host: None,
                placed_at: None,
                ..counted
            }
        };
        let freed = AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::FailedPrepare),
            ..allocation.clone()
        };
        let txn = put_job(Txn::new(), Some(&job), &after, Cond::Version(job_version))?;
        let txn = put_allocation(
            txn,
            Some(&allocation),
            &freed,
            Cond::Version(allocation_version),
        )?;
        let txn = self.without_mark(with_rows(txn, beside(&freed)), id)?;
        self.commit(id, txn)?;
        Ok((after, freed))
    }

    /// The prepare of a placed whole-host job starts again (an agent's hello): in one
    /// transaction the allocation row, at its version, takes `deadline` as its
    /// [`AllocationRecord::prepare_deadline`] and nothing else changes, with the job's row, at
    /// its version, a precondition. Returns the allocation as written.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is not
    /// placed; [`JobStoreError::NotWholeHost`]; [`JobStoreError::NoAllocation`];
    /// [`JobStoreError::AllocationNotBooked`] when the allocation is releasing or freed;
    /// [`JobStoreError::Stale`] when either row changed before the commit; as for any read and
    /// write.
    pub fn restart_prepare(
        &self,
        id: &JobId,
        deadline: StoreTime,
    ) -> Result<AllocationRecord, JobStoreError> {
        let (job, job_version) = self.placed_job(id)?;
        ensure(
            job.spec.needs == Needs::WholeHost,
            JobStoreError::NotWholeHost { id: id.clone() },
        )?;
        let (allocation, allocation_version) = self.booked_allocation(id)?;
        let restarted = AllocationRecord {
            prepare_deadline: Some(deadline),
            ..allocation.clone()
        };
        let txn = put_allocation(
            Txn::new(),
            Some(&allocation),
            &restarted,
            Cond::Version(allocation_version),
        )?;
        let txn = txn.check(keys::job(id), Cond::Version(job_version));
        self.commit(id, txn)?;
        Ok(restarted)
    }

    /// The job `id` and its allocation, each at the version read, for the failed prepare of a
    /// placed whole-host job: the job is placed, needs the whole host and has no cancel asked
    /// for (it is then cancelled, not requeued), and the allocation is booked (a releasing or
    /// freed one already ended, and its first end stands).
    pub(super) fn prepare_rows(&self, id: &JobId) -> Result<Rows, JobStoreError> {
        let (job, job_version) = self.placed_job(id)?;
        ensure(
            (job.spec.needs == Needs::WholeHost) & !job.cancel_requested,
            JobStoreError::IllegalTransition {
                id: id.clone(),
                from: job.state,
                to: JobState::Queued,
            },
        )?;
        let allocation = self.booked_allocation(id)?;
        Ok(((job, job_version), allocation))
    }

    /// The job `id` at the version read, when it is placed.
    fn placed_job(&self, id: &JobId) -> Result<(JobRecord, Version), JobStoreError> {
        let (job, version) = self.job_row(id)?;
        ensure(
            job.state == JobState::Placed,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Placed,
                found: job.state,
            },
        )?;
        Ok((job, version))
    }

    /// The allocation of the job `id` at the version read, when it is booked.
    fn booked_allocation(&self, id: &JobId) -> Result<(AllocationRecord, Version), JobStoreError> {
        let (allocation, version) = self.allocation_row(id)?;
        ensure(
            allocation.step == AllocationStep::Booked,
            JobStoreError::AllocationNotBooked {
                id: id.clone(),
                step: allocation.step,
            },
        )?;
        Ok((allocation, version))
    }
}
