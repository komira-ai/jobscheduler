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

use std::sync::atomic::Ordering;

use js_agent_wire::JobStartSpec;
use js_state::{Claim, Cond, Store, StoreTime, Txn, TxnRead, Version};
use js_types::{HostId, QueueNote};

use super::index::{put_allocation, put_job};
use super::{Beside, JobStore, allocation_at, booked_in, ensure, host_rows, job_at, with_rows};
use crate::codec::Carried;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{
    AllocationRecord, AllocationStep, Booked, Capacity, EndReason, Exclusion, JobRecord, JobState,
    Needs, StartFailure,
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
    /// The start the host's agent is sent for this placement, kept in the start message the
    /// placement queues for the host ([`crate::DispatchMessage::start`]); its job id, allocation
    /// id and placement time are set to the placement's own. `None`: the message carries no
    /// body. A whole-host placement queues no start (its job starts once its host is ready), so
    /// this is not kept for one.
    pub start: Option<JobStartSpec>,
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

    /// [`JobStore::place_with`] decided on the rows the placement's own transaction has
    /// locked, which is how a server places a queued job when other servers place too.
    ///
    /// The job's row is claimed first (`FOR UPDATE SKIP LOCKED` on Postgres): a job another
    /// transaction holds is [`JobStoreError::Stale`] at once and is left to it, without waiting.
    /// Then, in the same transaction, the job, its earlier allocation, its mark and the
    /// host's live allocations are read as they stand, so the decision is made on what the
    /// commit finds and not on a picture read earlier:
    ///
    /// - the job must still be queued (a placement by another server made it
    ///   [`JobStoreError::UnexpectedState`]);
    /// - with `capacity`, the host's live allocations plus this booking must fit it, else
    ///   [`JobStoreError::HostFull`] and nothing is written (the conditional update that
    ///   reserves the capacity: the booking and the check are one transaction);
    /// - the host must not be cordoned: a live `cordon/<host>` row is [`JobStoreError::HostCordoned`]
    ///   and nothing is written (the commit checks the row absent too);
    /// - the host must not be tainted: a live `taint/<host>` row, or a text `taint_reason` in
    ///   the host's binding row (`hosts/binding/<host>`), is [`JobStoreError::HostTainted`];
    /// - the host must not be marked to scale down: a `hostmark/<host>` row is
    ///   [`JobStoreError::HostMarked`];
    /// - none of the job's host ports may be held by a live allocation on the host, else
    ///   [`JobStoreError::PortTaken`];
    /// - the host's reservation row (`hostbook/<host>`) is written at the version the
    ///   transaction read, so a reader that works from a picture of the host sees it moved.
    ///
    /// Returns the job and the allocation as written.
    ///
    /// # Errors
    /// As [`JobStore::place_with`]; [`JobStoreError::Stale`] also when the job's row is held by
    /// another transaction or its commit's preconditions failed; [`JobStoreError::HostFull`];
    /// [`JobStoreError::HostCordoned`]; [`JobStoreError::HostTainted`];
    /// [`JobStoreError::HostMarked`]; [`JobStoreError::PortTaken`]. On a guarded view
    /// ([`JobStore::guarded`]) the commit also checks the node row, and a row that moved is
    /// [`JobStoreError::NodeMoved`].
    pub fn place_claimed(
        &self,
        id: &JobId,
        host: &HostId,
        booked: Booked,
        placement: &Placement,
        capacity: Option<&Capacity>,
    ) -> Result<(JobRecord, AllocationRecord), JobStoreError> {
        let mut failed = None;
        let mut placed = None;
        let claim = [keys::job(id)];
        // The store's time is read before the transaction opens: a call on the store from
        // inside the decision would need a second connection while the decision holds one.
        let now = self.store.now()?;
        let outcome = self.store.claim_decide(&claim, &mut |read| {
            let decided = self.decide_placement(read, id, (host, booked, placement), capacity, now);
            match decided {
                Ok((txn, job, allocation)) => {
                    let txn = self.guard_txn(txn);
                    placed = Some((job, allocation));
                    Some(txn)
                }
                Err(error) => {
                    failed = Some(error);
                    None
                }
            }
        });
        self.writes.fetch_add(1, Ordering::SeqCst);
        match (outcome?, placed, failed) {
            (Claim::Committed(_), Some(placed), _) => Ok(placed),
            (_, _, Some(error)) => Err(error),
            (Claim::Conflict(conflict), _, _) => Err(self
                .refuse_moved(conflict)
                .err()
                .unwrap_or(JobStoreError::Stale { id: id.clone() })),
            _ => Err(JobStoreError::Stale { id: id.clone() }),
        }
    }

    /// The transaction [`JobStore::place_claimed`] commits, decided on what `read` shows, with
    /// `now` the store's time it stamps the placement with. It reads only through `read`.
    fn decide_placement(
        &self,
        read: &mut dyn TxnRead,
        id: &JobId,
        (host, booked, placement): (&HostId, Booked, &Placement),
        capacity: Option<&Capacity>,
        now: StoreTime,
    ) -> Result<(Txn, JobRecord, AllocationRecord), JobStoreError> {
        let job_key = keys::job(id);
        let row = read
            .get(&job_key)?
            .ok_or(JobStoreError::NotFound { id: id.clone() })?;
        let job = job_at(&job_key, &row.value)?;
        let earlier_key = keys::allocation(id);
        let earlier = read
            .get(&earlier_key)?
            .map(|row| allocation_at(&earlier_key, &row.value).map(|found| (found, row.version)))
            .transpose()?;
        // The host's reservation row is read first, so the host's allocations are summed after
        // any placement ahead of this one on the host has committed, and not before (on Postgres
        // the decision runs while the store holds its commit head, which no placement shares).
        let book_key = keys::hostbook(host);
        let cond = read
            .get(&book_key)?
            .map_or(Cond::Absent, |row| Cond::Version(row.version));
        if let Some(capacity) = capacity {
            let booking = booked_in(read, host)?;
            ensure(
                capacity.has_room(&booking, &booked),
                JobStoreError::HostFull {
                    id: id.clone(),
                    host: host.clone(),
                },
            )?;
        }
        ensure(
            read.get(&keys::cordon(host))?.is_none(),
            JobStoreError::HostCordoned {
                id: id.clone(),
                host: host.clone(),
            },
        )?;
        let ports = job.spec.ports.iter().map(|port| port.host).collect();
        let (binding_key, binding_cond) = host_rows::refuse_host(read, (id, host), &ports)?;
        let (txn, placed, allocation) = self.build_placement(
            id,
            (job, row.version),
            earlier,
            (host.clone(), booked, placement.clone()),
            now,
        )?;
        let mark_key = keys::mark(id);
        let txn = match read.get(&mark_key)? {
            Some(row) => txn.delete(mark_key, Cond::Version(row.version)),
            None => txn.check(mark_key, Cond::Absent),
        };
        let txn = txn.put(book_key, allocation.allocation_id.to_string(), cond);
        let txn = txn.check(binding_key, binding_cond);
        Ok((txn, placed, allocation))
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
        let job = self.job_row(id)?;
        let earlier = self.read_allocation(id)?;
        let now = self.store.now()?;
        self.build_placement(id, job, earlier, (host, booked, placement), now)
    }

    /// The transaction that places the queued job `id`, read at `job`, on a host as
    /// [`JobStore::place`] describes, given its `earlier` allocation as read and the store's
    /// time `now`, and the job and allocation it writes. It makes no call on the store.
    fn build_placement(
        &self,
        id: &JobId,
        (job, job_version): (JobRecord, Version),
        earlier: Option<(AllocationRecord, Version)>,
        (host, booked, placement): (HostId, Booked, Placement),
        now: StoreTime,
    ) -> Result<(Txn, JobRecord, AllocationRecord), JobStoreError> {
        ensure(
            job.state == JobState::Queued,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Queued,
                found: job.state,
            },
        )?;
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
        let txn = super::dispatch::with_start(txn, &allocation, placement.start, now)?;
        // A host with a cordon in force, or marked to scale down, is placed on by no one: the
        // commit conflicts.
        let txn = txn.check(keys::cordon(&allocation.host), Cond::Absent);
        let txn = txn.check(keys::hostmark(&allocation.host), Cond::Absent);
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
    ///   precondition when there is none;
    /// - any start of the allocation is deleted from the host's dispatch queue, at the version
    ///   read (a placed whole-host job is queued none, and is sent no stop).
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
        let now = self.store.now()?;
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
        let txn = self.queue_stop(txn, &job, Some(&allocation), true, now)?;
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
    pub(super) fn booked_allocation(
        &self,
        id: &JobId,
    ) -> Result<(AllocationRecord, Version), JobStoreError> {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::{StoreError, TxnRead, Versioned};

    use super::*;
    use crate::ids::RandomIds;

    /// A view of a store that holds no row.
    struct Empty;

    impl TxnRead for Empty {
        fn get(&mut self, _key: &str) -> Result<Option<Versioned>, StoreError> {
            Ok(None)
        }

        fn scan_prefix(&mut self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
            Ok(Vec::new())
        }
    }

    /// Asserts that a decision made on a view that holds no row for the job says the job is
    /// not found, writing nothing. (The store claims the row before it decides, so a store
    /// never shows this; the decision does not rest on that.) It catches a decision that
    /// invents a job.
    #[test]
    fn a_decision_on_a_view_without_the_job_says_it_is_not_found() {
        let store = Arc::new(js_state::fake::Fake::with_flaw(
            Arc::new(js_state::ManualClock::default()),
            js_state::fake::Flaw::None,
        ));
        let jobs = JobStore::new(store, RandomIds).expect("opens");
        let id = JobId::parse("ghost").expect("an id");
        let host = HostId::new("host-a").expect("a host");
        assert_eq!(Empty.scan_prefix("x/"), Ok(Vec::new()));
        let decided = jobs.decide_placement(
            &mut Empty,
            &id,
            (&host, Booked::WholeHost, &Placement::default()),
            None,
            StoreTime::ZERO,
        );
        assert_eq!(decided.err(), Some(JobStoreError::NotFound { id }));
    }
}
