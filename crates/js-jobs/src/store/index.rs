//! The job store's secondary key spaces (`idx/`, see [`crate::keys`]): which entries a row
//! calls for, the operations that move a row's entries in the transaction that writes the row,
//! the indexed reads, and the backfill that makes the spaces agree with the tables when a job
//! store opens.
//!
//! An entry is a key with an empty value. Every write of a job or allocation row through the
//! job store moves the row's entries in the same transaction, so the spaces never disagree with
//! a row the job store wrote. A read through an entry reads the row it points at by key and
//! judges that row, not the entry: an entry whose row is gone is passed over. Rows that reach
//! the tables some other way (a release before this one, a later build, a hand) are the
//! backfill's: it runs whenever a job store opens, so a server's start indexes what the tables
//! hold.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use js_state::{Cond, ResumeToken, ScanRange, Store, Txn, Version, Versioned};

use super::{JobStore, allocation_at, job_at};
use crate::codec::{DecodeError, encode_allocation, encode_job};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, JobRecord};
use crate::result::SWEEP_BATCH;

/// What one backfill changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backfill {
    /// Entries it put: those a row called for and the spaces did not hold.
    pub added: usize,
    /// Entries it deleted: those the spaces held and no row called for.
    pub removed: usize,
}

/// The entries the job `job` calls for: active while it has not ended, and due while it has a
/// retention end.
pub(crate) fn job_entries(job: &JobRecord) -> BTreeSet<String> {
    let active = (!job.state.is_ended()).then(|| keys::active(&job.id));
    let due = job.retention_ends.map(|end| keys::due(end, &job.id));
    active.into_iter().chain(due).collect()
}

/// The entry the allocation `allocation` calls for: on its host while it is live (booked or
/// releasing), and freed, at its free, once it is freed. A freed row that holds no free time
/// counts from its booking, as the tick's collection does.
pub(crate) fn allocation_entries(allocation: &AllocationRecord) -> BTreeSet<String> {
    let entry = if allocation.step == AllocationStep::Freed {
        let at = allocation.freed_at.unwrap_or(allocation.created_at);
        keys::freed(at, &allocation.job)
    } else {
        keys::on_host(&allocation.host, &allocation.job)
    };
    BTreeSet::from([entry])
}

/// `txn` with the entries of `before` that `after` does not hold deleted, and those of `after`
/// that `before` does not hold put. The row the entries belong to carries the transaction's
/// precondition, so the entries take none.
fn moved(txn: Txn, before: &BTreeSet<String>, after: &BTreeSet<String>) -> Txn {
    let txn = before
        .difference(after)
        .fold(txn, |txn, entry| txn.delete(entry.as_str(), Cond::Any));
    after.difference(before).fold(txn, |txn, entry| {
        txn.put(entry.as_str(), Vec::new(), Cond::Any)
    })
}

/// `txn` with a put of `job` on `cond`, its entries moved from those of `before`, the record
/// the put replaces (`None` for a new row).
pub(crate) fn put_job(
    txn: Txn,
    before: Option<&JobRecord>,
    job: &JobRecord,
    cond: Cond,
) -> Result<Txn, JobStoreError> {
    let txn = txn.put(keys::job(&job.id), encode_job(job)?, cond);
    let before = before.map(job_entries).unwrap_or_default();
    Ok(moved(txn, &before, &job_entries(job)))
}

/// `txn` with a put of `allocation` on `cond`, its entries moved from those of `before`, the
/// record the put replaces (`None` for a new row).
pub(crate) fn put_allocation(
    txn: Txn,
    before: Option<&AllocationRecord>,
    allocation: &AllocationRecord,
    cond: Cond,
) -> Result<Txn, JobStoreError> {
    let txn = txn.put(
        keys::allocation(&allocation.job),
        encode_allocation(allocation)?,
        cond,
    );
    let before = before.map(allocation_entries).unwrap_or_default();
    Ok(moved(txn, &before, &allocation_entries(allocation)))
}

/// `txn` with a delete of `job`'s row at `version`, and of its entries.
pub(crate) fn delete_job(txn: Txn, job: &JobRecord, version: Version) -> Txn {
    let txn = txn.delete(keys::job(&job.id), Cond::Version(version));
    moved(txn, &job_entries(job), &BTreeSet::new())
}

/// `txn` with a delete of `allocation`'s row at `version`, and of its entries.
pub(crate) fn delete_allocation(txn: Txn, allocation: &AllocationRecord, version: Version) -> Txn {
    let txn = txn.delete(keys::allocation(&allocation.job), Cond::Version(version));
    moved(txn, &allocation_entries(allocation), &BTreeSet::new())
}

/// One index space to read: its prefix, and the first key past the entries wanted when only
/// the earlier part of the space is.
pub(crate) struct Span<'a> {
    /// The space's prefix (or a host's part of the on-host space).
    pub(crate) prefix: &'a str,
    /// Entries at or past this key are not read.
    pub(crate) below: Option<String>,
}

impl<'a> Span<'a> {
    /// Every entry under `prefix`.
    pub(crate) fn all(prefix: &'a str) -> Span<'a> {
        Span {
            prefix,
            below: None,
        }
    }
}

/// The row key of `entry`, whose rows a later build's entry does not name.
fn row_of(entry: &str) -> Option<String> {
    keys::indexed_row(entry)
}

/// The id of the job a `job/<id>` or `alloc/<id>` key names, if it is a valid one.
fn id_of(key: &str) -> Option<JobId> {
    let rest = key
        .strip_prefix(keys::JOB)
        .or_else(|| key.strip_prefix(keys::ALLOCATION))?;
    JobId::parse(rest).ok()
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// A page of at most `limit` entries of `span`, from the start or past `resume`, in key
    /// order, and where the next page begins.
    pub(crate) fn entry_page(
        &self,
        span: &Span<'_>,
        resume: Option<&ResumeToken>,
        limit: std::num::NonZeroUsize,
    ) -> Result<(Vec<String>, Option<ResumeToken>), JobStoreError> {
        let mut scan = ScanRange::new(span.prefix, limit);
        if let Some(below) = &span.below {
            scan.upper = Bound::Excluded(below.as_str());
        }
        let scan = match resume {
            Some(token) => scan.resume(token),
            None => scan,
        };
        let page = self.store.scan_range(scan)?;
        let entries = page.rows.into_iter().map(|(key, _)| key).collect();
        Ok((entries, page.resume))
    }

    /// Every entry of `span`, in key order.
    fn entries(&self, span: &Span<'_>) -> Result<Vec<String>, JobStoreError> {
        let mut all = Vec::new();
        let mut resume = None;
        loop {
            let (entries, next) = self.entry_page(span, resume.as_ref(), SWEEP_BATCH)?;
            all.extend(entries);
            match next {
                Some(token) => resume = Some(token),
                None => return Ok(all),
            }
        }
    }

    /// The rows `entries` point at, each once, in key order, read by key; an entry whose row
    /// is gone is passed over.
    pub(crate) fn rows_of(
        &self,
        entries: impl IntoIterator<Item = String>,
    ) -> Result<Vec<(String, Versioned)>, JobStoreError> {
        let keys: BTreeSet<String> = entries.into_iter().filter_map(|e| row_of(&e)).collect();
        let mut rows = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(row) = self.store.get(&key)? {
                rows.push((key, row));
            }
        }
        Ok(rows)
    }

    /// The rows the entries of `spans` point at, each once, in key order: what an indexed read
    /// judges in place of every row of a table.
    pub(crate) fn indexed_rows(
        &self,
        spans: &[Span<'_>],
    ) -> Result<Vec<(String, Versioned)>, JobStoreError> {
        let mut entries = Vec::new();
        for span in spans {
            entries.extend(self.entries(span)?);
        }
        self.rows_of(entries)
    }

    /// Makes the index spaces agree with the tables (the module docs): reads every job and
    /// allocation row and every entry, then, for each row whose entries differ from those it
    /// calls for, deletes the entries it does not call for and puts those it does, in
    /// transactions of about [`SWEEP_BATCH`] operations, each with the version of every row
    /// it judged as a precondition. A row this build cannot read calls for one `unread` entry,
    /// which every indexed read of its table reads, so it is judged as a scan would judge it.
    /// An entry of a space this build does not name is left as it is. A second backfill with
    /// nothing written in between changes nothing and commits nothing.
    ///
    /// [`JobStore::new`] runs it, so every job store opened, a server's at its start included,
    /// reads indexes that agree with its tables.
    ///
    /// # Errors
    /// [`JobStoreError::Stale`] when a row changed between the read and its batch's commit
    /// (the batches before stay committed, and the next backfill finishes the work);
    /// [`JobStoreError::Corrupt`] when that row's key names no valid job id;
    /// [`JobStoreError::Store`].
    pub fn backfill_indexes(&self) -> Result<Backfill, JobStoreError> {
        let wanted = self.wanted_entries()?;
        let stored = self.stored_entries()?;
        let rows: BTreeSet<&String> = wanted.keys().chain(stored.keys()).collect();
        let none = BTreeSet::new();
        let mut done = Backfill::default();
        let mut txn = Txn::new();
        for row in rows {
            let (cond, want) = match wanted.get(row) {
                Some((version, entries)) => (Cond::Version(*version), entries),
                None => (Cond::Absent, &none),
            };
            let have = stored.get(row).unwrap_or(&none);
            if want == have {
                continue;
            }
            done.added += want.difference(have).count();
            done.removed += have.difference(want).count();
            txn = moved(txn.check(row.as_str(), cond), have, want);
            if txn.ops().len() >= SWEEP_BATCH.get() {
                self.commit_backfill(std::mem::take(&mut txn))?;
            }
        }
        if !txn.ops().is_empty() {
            self.commit_backfill(txn)?;
        }
        Ok(done)
    }

    /// Commits one batch of a backfill; a lost precondition names the row it was on.
    fn commit_backfill(&self, txn: Txn) -> Result<(), JobStoreError> {
        match self.store.txn(txn)? {
            Ok(_) => Ok(()),
            Err(conflict) => Err(match id_of(&conflict.key) {
                Some(id) => JobStoreError::Stale { id },
                None => JobStoreError::Corrupt {
                    key: conflict.key,
                    why: DecodeError::WrongKey,
                },
            }),
        }
    }

    /// Every job and allocation row with its version and the entries it calls for, read by a
    /// scan of both tables: the reference the index spaces must agree with.
    fn wanted_entries(
        &self,
    ) -> Result<BTreeMap<String, (Version, BTreeSet<String>)>, JobStoreError> {
        let mut wanted = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(keys::JOB)? {
            let entries = match job_at(&key, &row.value) {
                Ok(job) => job_entries(&job),
                Err(_) => keys::unread(&key).into_iter().collect(),
            };
            wanted.insert(key, (row.version, entries));
        }
        for (key, row) in self.store.scan_prefix(keys::ALLOCATION)? {
            let entries = match allocation_at(&key, &row.value) {
                Ok(allocation) => allocation_entries(&allocation),
                Err(_) => keys::unread(&key).into_iter().collect(),
            };
            wanted.insert(key, (row.version, entries));
        }
        Ok(wanted)
    }

    /// Every entry of the spaces this build names, by the row it points at.
    fn stored_entries(&self) -> Result<BTreeMap<String, BTreeSet<String>>, JobStoreError> {
        let mut stored: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (entry, _) in self.store.scan_prefix(keys::INDEX)? {
            if let Some(row) = row_of(&entry) {
                stored.entry(row).or_default().insert(entry);
            }
        }
        Ok(stored)
    }

    /// The entries the job and allocation rows call for, by a full scan of both tables: the
    /// reference an index check compares [`JobStore::index_entries`] with. Tests only; the
    /// store's reads never scan a table.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn index_by_scan(&self) -> Result<BTreeSet<String>, JobStoreError> {
        Ok(self
            .wanted_entries()?
            .into_values()
            .flat_map(|(_, entries)| entries)
            .collect())
    }

    /// Every entry the index spaces this build names hold. Tests only.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn index_entries(&self) -> Result<BTreeSet<String>, JobStoreError> {
        Ok(self.stored_entries()?.into_values().flatten().collect())
    }
}
