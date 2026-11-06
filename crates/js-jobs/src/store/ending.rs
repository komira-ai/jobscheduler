//! Ending a job with its result and retention end, deleting the jobs past their retention
//! end, deleting the freed allocations past their own window, and raising retention ends and
//! deadline floors at boot (design sections 3.2, 6.1 and 6.7). Apart from the release of a
//! booked allocation in the transaction that ends its job, the only allocation rows written
//! here are the freed ones [`JobStore::tick`] deletes: a freed row is kept for the store's
//! allocation retention after its free ([`JobStore::with_allocation_retention`]) and then
//! deleted, at most [`SWEEP_BATCH`] rows a tick. The usage history of an allocation is the
//! `usage/ended` row written beside its free, which no retention here touches; a booked or
//! releasing row is never deleted.

use js_state::{Cond, ResumeToken, Store, StoreTime, Txn, Version};

use std::time::Duration;

use super::index::{Span, delete_allocation, delete_job, put_allocation, put_job};
use super::{JobStore, allocation_at, ensure, job_at, same_key};
use crate::codec::{DecodeError, decode_job};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, JobRecord, may_move};
use crate::result::{
    BOOT_GRACE, JobEnd, Retention, SWEEP_BATCH, digested_at_boot, ended, floored_at_boot,
    past_retention, raised_at_boot,
};

/// Whether `allocation` is freed and its free is at least `retention` before `now`, so the
/// tick may delete its row. A freed row that holds no free time (none this build writes)
/// counts from its booking. A booked or releasing allocation is never collectable.
pub(crate) fn collectable(
    allocation: &AllocationRecord,
    retention: Retention,
    now: StoreTime,
) -> bool {
    let since = allocation.freed_at.unwrap_or(allocation.created_at);
    (allocation.step == AllocationStep::Freed) & (since.saturating_add(retention.window()) <= now)
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Ends the job `id` as `end` says (design section 3.2): in one transaction the job takes
    /// its end state, its end time, its result (none for a job that was lost or failed to
    /// start; each log tail longer than the cap cut to its last bytes on a character boundary
    /// and marked truncated) and its retention end, the store's clock now plus the store's
    /// [`Retention`](crate::Retention); a booked allocation of the job moves to releasing in
    /// the same transaction (design section 8, row 25), so the job and its allocation never
    /// disagree. Every end goes through here: [`JobStore::transition`] makes no end, and the
    /// cancel of a queued job ([`JobStore::request_cancel`]) ends it here too.
    ///
    /// Ending a job that has already ended writes nothing and returns the stored record: the
    /// first result and retention end stand.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::IllegalTransition`] when the job's state
    /// cannot end that way (an exit of a job that is not running, a start failure from a state
    /// the design does not name for it); [`JobStoreError::Stale`] when the job or its
    /// allocation changed before the commit; as for any read and write.
    pub fn end_job(&self, id: &JobId, end: JobEnd) -> Result<JobRecord, JobStoreError> {
        let (job, version) = self.job_row(id)?;
        if job.state.is_ended() {
            return Ok(job);
        }
        let (txn, ended) = self.end_txn(job, version, end, self.store.now())?;
        self.commit(id, txn)?;
        Ok(ended)
    }

    /// The transaction that ends `job`, read at `version`, as `end` says at `now`, and the
    /// record it writes: the job's row at its version and, when the job's allocation is
    /// booked, the allocation's row at its version, moved to releasing.
    pub(super) fn end_txn(
        &self,
        job: JobRecord,
        version: Version,
        end: JobEnd,
        now: StoreTime,
    ) -> Result<(Txn, JobRecord), JobStoreError> {
        let to = end.state();
        ensure(
            may_move(job.state, to),
            JobStoreError::IllegalTransition {
                id: job.id.clone(),
                from: job.state,
                to,
            },
        )?;
        let before = job.clone();
        let ended = ended(job, end, now, self.retention);
        let id = &ended.id;
        let mut txn = put_job(Txn::new(), Some(&before), &ended, Cond::Version(version))?;
        if let Some((allocation, version)) = self.read_allocation(id)?
            && allocation.step == AllocationStep::Booked
        {
            let releasing = AllocationRecord {
                step: AllocationStep::Releasing,
                ..allocation.clone()
            };
            txn = put_allocation(txn, Some(&allocation), &releasing, Cond::Version(version))?;
        }
        Ok((txn, ended))
    }

    /// Deletes, in one transaction, the rows of the jobs among `ids` that are past their
    /// retention end by the store's clock now, as [`sweep`](crate::sweep) lists them, and
    /// returns their ids. Each row is read again first and deleted at the version read, so a
    /// job whose retention end was raised since it was swept, or that is gone, is left as it
    /// is. Only job rows are deleted: an allocation row stays as history.
    ///
    /// # Errors
    /// [`JobStoreError::Stale`] (naming the first job of the transaction) when a row changed
    /// between its read and the commit, and nothing is deleted; [`JobStoreError::UnknownRow`]
    /// or [`JobStoreError::Corrupt`] when a row cannot be read; [`JobStoreError::Store`].
    pub fn delete_swept(&self, ids: &[JobId]) -> Result<Vec<JobId>, JobStoreError> {
        let mut read = Vec::new();
        for id in ids {
            read.extend(self.read_job(id)?);
        }
        self.delete_due(read, self.store.now())
    }

    /// One pass of the retention sweep (design section 6.1, step 1): walks the job table in
    /// key order, [`SWEEP_BATCH`](crate::SWEEP_BATCH) rows at a time, and deletes the jobs
    /// of each batch that are past their retention end by the store's clock, read once at
    /// the start of the pass, one transaction per batch. It reads no system time: the time is
    /// the store's [`Clock`](js_state::Clock). A row a later build wrote is left as it is and
    /// logged. It returns the ids it deleted.
    ///
    /// Then it deletes at most one batch of freed allocation rows whose free is the store's
    /// allocation retention or more before that time ([`JobStore::with_allocation_retention`]),
    /// in one transaction, each at the version read, so a row placed again since is kept; a
    /// backlog larger than a batch is cleared over the ticks that follow. A batch that loses to
    /// such a change deletes nothing and the next tick reads again. A booked or releasing row,
    /// and a row this build cannot read, is never deleted.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a job row does not parse; [`JobStoreError::Stale`] when
    /// a job row of a batch changed before its commit (the batches before it stay deleted);
    /// [`JobStoreError::Store`].
    pub fn tick(&self) -> Result<Vec<JobId>, JobStoreError> {
        let now = self.store.now();
        let deleted = self.sweep_jobs(now)?;
        let collected = self.collect_allocations(now)?;
        if !collected.is_empty() {
            tracing::info!(
                collected = collected.len(),
                "freed allocations past their retention deleted"
            );
        }
        Ok(deleted)
    }

    /// The job half of [`JobStore::tick`]: every job past its retention end at `now`, deleted
    /// one transaction per batch.
    fn sweep_jobs(&self, now: StoreTime) -> Result<Vec<JobId>, JobStoreError> {
        let due = Span {
            prefix: keys::DUE,
            below: Some(keys::due_below(now.saturating_add(Duration::from_nanos(1)))),
        };
        let mut deleted = Vec::new();
        let mut resume: Option<ResumeToken> = None;
        loop {
            let (entries, next) = self.entry_page(&due, resume.as_ref(), SWEEP_BATCH)?;
            let read = self.sweepable(self.rows_of(entries)?)?;
            deleted.extend(self.delete_due(read, now)?);
            match next {
                Some(token) => resume = Some(token),
                None => break,
            }
        }
        // The rows this build cannot read, judged as a scan of the table would judge them.
        let unread = self.indexed_rows(&[Span::all(keys::UNREAD_JOB)])?;
        let read = self.sweepable(unread)?;
        deleted.extend(self.delete_due(read, now)?);
        Ok(deleted)
    }

    /// The jobs of `rows` with their versions; a row a later build wrote is left out and
    /// logged.
    fn sweepable(
        &self,
        rows: Vec<(String, js_state::Versioned)>,
    ) -> Result<Vec<(JobRecord, Version)>, JobStoreError> {
        let mut read = Vec::new();
        for (key, row) in rows {
            match decode_job(&row.value).and_then(|job| same_key(&key, keys::job(&job.id), job)) {
                Ok(job) => read.push((job, row.version)),
                Err(DecodeError::Newer) => {
                    tracing::warn!(%key, "a job row a later build wrote is left to its retention");
                }
                Err(why) => return Err(JobStoreError::Corrupt { key, why }),
            }
        }
        Ok(read)
    }

    /// Deletes, in one transaction at the versions read, the jobs of `read` past their
    /// retention end at `now`, and returns their ids; with none, commits nothing.
    fn delete_due(
        &self,
        read: Vec<(JobRecord, Version)>,
        now: StoreTime,
    ) -> Result<Vec<JobId>, JobStoreError> {
        let mut due = Vec::new();
        let mut txn = Txn::new();
        for (job, version) in read {
            if past_retention(&job, now) {
                txn = delete_job(txn, &job, version);
                due.push(job.id);
            }
        }
        if let Some(first) = due.first() {
            self.commit(first, txn)?;
        }
        Ok(due)
    }

    /// The allocation half of [`JobStore::tick`]: deletes, in one transaction at the versions
    /// read, up to [`SWEEP_BATCH`] freed allocations collectable at `now` ([`collectable`]),
    /// and returns their jobs' ids. With none, or when the transaction loses to a change of one
    /// of its rows, it commits nothing and returns none.
    fn collect_allocations(&self, now: StoreTime) -> Result<Vec<JobId>, JobStoreError> {
        let window = self.allocation_retention.window();
        let Some(oldest) = now.as_duration().checked_sub(window) else {
            return Ok(Vec::new());
        };
        let oldest = StoreTime::from_duration(oldest).saturating_add(Duration::from_nanos(1));
        let freed = Span {
            prefix: keys::FREED,
            below: Some(keys::freed_below(oldest)),
        };
        let (entries, _) = self.entry_page(&freed, None, SWEEP_BATCH)?;
        let mut due = Vec::new();
        let mut txn = Txn::new();
        for (key, row) in self.rows_of(entries)? {
            // A row this build cannot read is not known to be freed: it is kept.
            let Ok(allocation) = allocation_at(&key, &row.value) else {
                continue;
            };
            if collectable(&allocation, self.allocation_retention, now) {
                txn = delete_allocation(txn, &allocation, row.version);
                due.push(allocation.job);
            }
        }
        if due.is_empty() || self.store.txn(txn)?.is_err() {
            return Ok(Vec::new());
        }
        Ok(due)
    }

    /// `txn` with every stored retention end raised to at least `boot` plus
    /// [`BOOT_GRACE`](crate::BOOT_GRACE) (design section 6.7, step 7): an end earlier than
    /// that is put at that instant, and the job's row is written at the version read; a later
    /// end, and a job with none (one that has not ended), is left as it is. Nothing else of a
    /// record changes: its result, its state and its other times are kept. The raise is from
    /// the boot, never from the stored end plus the grace, so restarts in a loop do not
    /// stretch a window, and it never lowers an end. It runs before the first
    /// [`JobStore::tick`]; the caller commits `txn`, which may carry the other deadlines it
    /// raises at the same boot. A row a later build wrote is left as it is.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse; [`JobStoreError::Store`].
    pub fn raise_deadlines_at_boot(&self, txn: Txn, boot: StoreTime) -> Result<Txn, JobStoreError> {
        let mut txn = txn;
        let ended = Span {
            prefix: keys::DUE,
            below: Some(keys::due_below(boot.saturating_add(BOOT_GRACE))),
        };
        let spans = [ended, Span::all(keys::UNREAD_JOB)];
        for (key, row) in self.indexed_rows(&spans)? {
            let job = match job_at(&key, &row.value) {
                Ok(job) => job,
                Err(JobStoreError::UnknownRow { .. }) => continue,
                Err(error) => return Err(error),
            };
            if let Some(raised) = raised_at_boot(&job, boot) {
                txn = put_job(txn, Some(&job), &raised, Cond::Version(row.version))?;
            }
        }
        Ok(txn)
    }

    /// The boot of a server (design section 6.7, step 7), committed in one transaction before
    /// the first tick: each stored retention end is raised as
    /// [`JobStore::raise_deadlines_at_boot`] raises it, and each job that waits to be placed or
    /// to start (queued, placed or starting) gets a deadline floor of at least `boot` plus
    /// [`BOOT_GRACE`](crate::BOOT_GRACE), the earliest instant its queue wait or its start
    /// timeout may end it. A floor is raised to that instant, never by it, so boots in a loop
    /// do not stretch a wait, and a later floor is kept. An ended job whose row still holds
    /// its files' contents (one ended by a build that kept them) has them replaced by their
    /// digests ([`crate::JobFiles`]), in the same put as its raised retention end when it has
    /// one. Only an ended job has a retention end or contents to replace and only a waiting
    /// one gets a floor, so no row is put twice. A row a later build wrote is left as it is.
    /// Returns how many rows it wrote; with none it commits nothing.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse; [`JobStoreError::Stale`] (naming
    /// the first job written) when a row changed between its read and the commit, and nothing
    /// is written; [`JobStoreError::Store`].
    pub fn raise_at_boot(&self, boot: StoreTime) -> Result<usize, JobStoreError> {
        let mut txn = Txn::new();
        let mut first: Option<JobId> = None;
        let mut written = 0;
        // Every ended job, not only those whose end is raised: any of them may still hold its
        // files' contents to replace. Every ended job has a retention end, so a due entry.
        let spans = [
            Span::all(keys::DUE),
            Span::all(keys::ACTIVE),
            Span::all(keys::UNREAD_JOB),
        ];
        for (key, row) in self.indexed_rows(&spans)? {
            let job = match job_at(&key, &row.value) {
                Ok(job) => job,
                Err(JobStoreError::UnknownRow { .. }) => continue,
                Err(error) => return Err(error),
            };
            let raised = match raised_at_boot(&job, boot) {
                Some(raised) => Some(digested_at_boot(&raised).unwrap_or(raised)),
                None => floored_at_boot(&job, boot).or_else(|| digested_at_boot(&job)),
            };
            if let Some(raised) = raised {
                first.get_or_insert_with(|| raised.id.clone());
                txn = put_job(txn, Some(&job), &raised, Cond::Version(row.version))?;
                written += 1;
            }
        }
        if let Some(first) = first {
            self.commit(&first, txn)?;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::time::Duration;

    use js_state::ManualClock;
    use js_state::fake::{Fake, Flaw};

    use super::super::tests::Lines;
    use crate::ids::RandomIds;
    use crate::suite::{full_spec, host, share};
    use crate::{EndReason, JobId, JobStore, NewJob, Retention};

    // Catches a collection that deletes rows without a word: the tick that deletes a freed
    // allocation logs how many it deleted.
    #[test]
    fn a_tick_that_collects_allocations_says_how_many() {
        let lines = Lines::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(lines.clone())
            .with_ansi(false)
            .finish();
        let clock = Arc::new(ManualClock::default());
        let store = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
        let window = Retention::new(Duration::from_secs(60)).expect("a window");
        let jobs = JobStore::new(store, RandomIds)
            .expect("opens")
            .with_allocation_retention(window);
        let id = JobId::parse("logged").expect("an id");
        let job = NewJob {
            id: Some(id.clone()),
            spec: full_spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(job).expect("queues");
        jobs.place(&id, host("host-a"), share()).expect("places");
        jobs.free(&id, EndReason::Exited).expect("frees");
        clock.advance(window.window());
        let ticked = tracing::subscriber::with_default(subscriber, || jobs.tick());
        assert_eq!(ticked, Ok(Vec::new()));
        assert_eq!(jobs.get_allocation(&id), Ok(None));
        lines.clone().flush().expect("flushes");
        let said = String::from_utf8(lines.0.lock().expect("unpoisoned").clone()).expect("text");
        assert!(
            said.contains("freed allocations past their retention deleted")
                & said.contains("collected=1"),
            "{said}"
        );
    }
}
