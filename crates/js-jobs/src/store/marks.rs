//! The mark table (`mark/<id>`, [`MarkRecord`]): the host a queued whole-host job holds while
//! that host drains, and the hosts it is kept off (design section 5.4). A mark row is written
//! only for a job that is queued and needs the whole host, with the job's version as a
//! precondition, and [`JobStore::end_and_unmark`] ends a job and deletes its mark in one
//! transaction, so a mark is never left behind by an end made that way, nor written after it.
//!
//! Three writes drop a job's mark in the transaction that writes the job: [`JobStore::place_with`]
//! (the job is placed), [`JobStore::request_cancel`] of a queued job (the job is cancelled) and
//! [`JobStore::fail_prepare_with`] (a mark left on a placed job does not outlive its failed
//! prepare). Each deletes `mark/<id>` at the version it read, or makes the row's absence a
//! precondition when there is none, so a mark written in between makes the write stale rather
//! than leaving a mark on a job that is not queued. The other writes of a job
//! ([`JobStore::end_job`], [`JobStore::place`]) do not touch its mark: the caller that keeps
//! marks ends a marked job with [`JobStore::end_and_unmark`] and deletes the mark of one it
//! places with `place`, or whose mark it drops, with [`JobStore::delete_mark`].
//!
//! The table is additive: a store written before it has no `mark/` rows and reads and writes
//! its jobs and allocations as before; no `idx/` space names a mark, so the index backfill
//! neither reads nor writes one. A job has one mark at most, and only while it waits for the
//! host it holds, so the table is read whole, by a scan of `mark/`.

use js_state::{Cond, Store, StoreTime, Txn, Version};

use super::{JobStore, ensure, same_key};
use crate::codec::{Carried, decode_mark, encode_mark};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{JobRecord, JobState, MarkRecord, Needs};
use crate::result::{BOOT_GRACE, JobEnd};

/// The mark the row `key` holds.
fn mark_at(key: &str, value: &[u8]) -> Result<MarkRecord, JobStoreError> {
    decode_mark(value)
        .and_then(|mark| same_key(key, keys::mark(&mark.job), mark))
        .map_err(|why| JobStoreError::row(key, why))
}

/// `mark` with its deadline raised to `boot` plus [`BOOT_GRACE`] when it is earlier; `None`
/// when it is at least that late. The raise is a maximum, never the stored deadline plus the
/// grace, so boots in a loop do not stretch a hold. Nothing else changes: the exclusions keep
/// their ends.
pub(crate) fn marked_at_boot(mark: &MarkRecord, boot: StoreTime) -> Option<MarkRecord> {
    let floor = boot.saturating_add(BOOT_GRACE);
    (mark.deadline < floor).then(|| MarkRecord {
        deadline: floor,
        ..mark.clone()
    })
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Writes `mark` as the mark of its job: in one transaction, with the version of the job's
    /// row as a precondition, the row `mark/<id>` is put (if absent, or at the version of the
    /// mark it replaces). The job must exist, be queued and need the whole host. A mark that
    /// replaces one on the same host keeps the fields a later build wrote to that row (the
    /// fields `mark` carries win); a mark on another host is a new mark and keeps none, as a
    /// new placement's allocation keeps none of the freed one it replaces. Returns what it
    /// wrote.
    ///
    /// # Errors
    /// [`JobStoreError::NotFound`]; [`JobStoreError::UnexpectedState`] when the job is not
    /// queued; [`JobStoreError::NotWholeHost`]; [`JobStoreError::UnknownRow`] or
    /// [`JobStoreError::Corrupt`] when the job's row or the mark row it replaces cannot be
    /// read; [`JobStoreError::Stale`] when either row changed before the commit;
    /// [`JobStoreError::TooLarge`]; [`JobStoreError::Store`].
    pub fn put_mark(&self, mark: MarkRecord) -> Result<MarkRecord, JobStoreError> {
        let id = mark.job.clone();
        let (job, job_version) = self.job_row(&id)?;
        ensure(
            job.state == JobState::Queued,
            JobStoreError::UnexpectedState {
                id: id.clone(),
                expected: JobState::Queued,
                found: job.state,
            },
        )?;
        ensure(
            job.spec.needs == Needs::WholeHost,
            JobStoreError::NotWholeHost { id: id.clone() },
        )?;
        let (cond, written) = match self.read_mark(&id)? {
            None => (Cond::Absent, mark),
            Some((stored, version)) => {
                let mut carried = if stored.host == mark.host {
                    stored.carried
                } else {
                    Carried::default()
                };
                carried.0.extend(mark.carried.0);
                (Cond::Version(version), MarkRecord { carried, ..mark })
            }
        };
        let txn = Txn::new()
            .check(keys::job(&id), Cond::Version(job_version))
            .put(keys::mark(&id), encode_mark(&written)?, cond);
        self.commit(&id, txn)?;
        Ok(written)
    }

    /// The mark of the job `id`, if it has one.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] when a later build wrote the row,
    /// [`JobStoreError::Corrupt`] when it does not parse, [`JobStoreError::Store`].
    pub fn get_mark(&self, id: &JobId) -> Result<Option<MarkRecord>, JobStoreError> {
        Ok(self.read_mark(id)?.map(|(mark, _)| mark))
    }

    /// Every mark, in job id order. A row a later build wrote is left out and logged.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse; [`JobStoreError::Store`].
    pub fn marks(&self) -> Result<Vec<MarkRecord>, JobStoreError> {
        let mut marks = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::MARK)? {
            match mark_at(&key, &row.value) {
                Ok(mark) => marks.push(mark),
                Err(JobStoreError::UnknownRow { key }) => {
                    tracing::warn!(%key, "a mark row a later build wrote is left out of the list");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(marks)
    }

    /// Deletes the mark of the job `id`, at the version read, whatever the row holds (a later
    /// build's row included: a delete keeps nothing of it to lose). Returns whether there was
    /// one; with none, nothing is written.
    ///
    /// # Errors
    /// [`JobStoreError::Stale`] when the row changed before the commit;
    /// [`JobStoreError::Store`].
    pub fn delete_mark(&self, id: &JobId) -> Result<bool, JobStoreError> {
        let key = keys::mark(id);
        let Some(row) = self.store.get(&key)? else {
            return Ok(false);
        };
        self.commit(id, Txn::new().delete(key, Cond::Version(row.version)))?;
        Ok(true)
    }

    /// Ends the job `id` as [`JobStore::end_job`] ends it and deletes its mark, in one
    /// transaction: the job's end (and the release of a booked allocation) as `end_job` writes
    /// it, and a delete of `mark/<id>` at the version read, whatever the row holds; with no
    /// mark, the row's absence is a precondition, so a mark written in between makes the end
    /// stale rather than leaving it behind. Both happen or neither does. The cancel of a
    /// marked job is this call with [`JobEnd::Cancelled`].
    ///
    /// A job that has already ended keeps its first end, and only its mark, if it has one, is
    /// deleted; with no mark either, nothing is written. Returns the job as it is after the
    /// call.
    ///
    /// # Errors
    /// As [`JobStore::end_job`]; [`JobStoreError::Stale`] also when the mark row changed (or
    /// was written) before the commit.
    pub fn end_and_unmark(&self, id: &JobId, end: JobEnd) -> Result<JobRecord, JobStoreError> {
        let (job, version) = self.job_row(id)?;
        let key = keys::mark(id);
        let mark = self.store.get(&key)?;
        let (txn, job) = if job.state.is_ended() {
            (Txn::new(), job)
        } else {
            self.end_txn(job, version, end, self.store.now())?
        };
        let txn = match mark {
            Some(row) => txn.delete(key, Cond::Version(row.version)),
            None if txn.ops().is_empty() => return Ok(job),
            None => txn.check(key, Cond::Absent),
        };
        self.commit(id, txn)?;
        Ok(job)
    }

    /// `txn` with the mark of the job `id` deleted, at the version read, whatever the row holds;
    /// when there is none, with the row's absence a precondition of `txn`, so a mark written
    /// between this read and the commit makes the commit fail.
    pub(super) fn without_mark(&self, txn: Txn, id: &JobId) -> Result<Txn, JobStoreError> {
        let key = keys::mark(id);
        Ok(match self.store.get(&key)? {
            Some(row) => txn.delete(key, Cond::Version(row.version)),
            None => txn.check(key, Cond::Absent),
        })
    }

    /// `txn` with every mark whose deadline is before `boot` plus [`BOOT_GRACE`] put with its
    /// deadline raised to that instant ([`marked_at_boot`]), each at the version read, and the
    /// jobs of the marks it raised. A row a later build wrote is left as it is.
    pub(super) fn raise_marks_at_boot(
        &self,
        txn: Txn,
        boot: StoreTime,
    ) -> Result<(Txn, Vec<JobId>), JobStoreError> {
        let mut txn = txn;
        let mut raised = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::MARK)? {
            let mark = match mark_at(&key, &row.value) {
                Ok(mark) => mark,
                Err(JobStoreError::UnknownRow { .. }) => continue,
                Err(error) => return Err(error),
            };
            if let Some(later) = marked_at_boot(&mark, boot) {
                txn = txn.put(key, encode_mark(&later)?, Cond::Version(row.version));
                raised.push(later.job);
            }
        }
        Ok((txn, raised))
    }

    fn read_mark(&self, id: &JobId) -> Result<Option<(MarkRecord, Version)>, JobStoreError> {
        let key = keys::mark(id);
        self.store
            .get(&key)?
            .map(|row| Ok((mark_at(&key, &row.value)?, row.version)))
            .transpose()
    }
}
