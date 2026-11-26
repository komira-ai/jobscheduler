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

use js_state::{Cond, ResumeToken, ScanRange, Store, StoreTime, Txn, Version, Versioned};

use super::{JobStore, allocation_at, job_at};
use crate::codec::{encode_allocation, encode_attempt_row, encode_job};
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, JobRecord, JobState};
use crate::result::SWEEP_BATCH;

/// What one backfill changed: what its committed transactions put and deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backfill {
    /// Entries it put: those a row called for and the spaces did not hold when it read them
    /// (for a row another writer changed meanwhile, which it read again, every entry the row
    /// calls for).
    pub added: usize,
    /// Entries it deleted: those the spaces held and no row called for when it read them (for
    /// a row read again, every entry it knew of that the row no longer calls for).
    pub removed: usize,
}

/// The entries the job `job` calls for: active while it has not ended, and queued too while it
/// is queued; due once it has ended, at its retention end (only an ended job has one). An ended
/// row with no retention end (one an earlier build wrote) is due at the origin, so every ended
/// job is under [`keys::DUE`]; the sweep never deletes such a row, since it has no end to be
/// past.
pub(crate) fn job_entries(job: &JobRecord) -> BTreeSet<String> {
    if job.state.is_ended() {
        let due = keys::due(job.retention_ends.unwrap_or(StoreTime::ZERO), &job.id);
        return BTreeSet::from([due]);
    }
    let queued = (job.state == JobState::Queued).then(|| keys::queued(&job.id));
    queued.into_iter().chain([keys::active(&job.id)]).collect()
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
/// the put replaces (`None` for a new row), and a put of the row of each attempt `job` has
/// ended that `before` had not ([`keys::attempt`]): the rows go in the commit that ends or
/// retries their attempts, whatever the key held (a row of an earlier job of the same id).
pub(crate) fn put_job(
    txn: Txn,
    before: Option<&JobRecord>,
    job: &JobRecord,
    cond: Cond,
) -> Result<Txn, JobStoreError> {
    let txn = txn.put(keys::job(&job.id), encode_job(job)?, cond);
    let known = before.map_or(0, |before| before.attempt.ended());
    let ended = job.attempt.history().iter();
    let rows: Result<Vec<_>, JobStoreError> = ended
        .filter(|attempt| u64::from(attempt.number) > known)
        .map(|attempt| {
            let key = keys::attempt(&job.id, attempt.number);
            encode_attempt_row(&job.id, attempt).map(|row| (key, row))
        })
        .collect();
    let put = |txn: Txn, (key, row): (String, Vec<u8>)| txn.put(key, row, Cond::Any);
    let txn = rows?.into_iter().fold(txn, put);
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

/// Logs, when the backfill read `again` rows again, one line saying so with their count.
pub(super) fn said_again(again: usize) {
    if again > 0 {
        tracing::info!(
            again,
            "the index backfill read again the rows another server changed while it ran"
        );
    }
}

/// The entries the row `value` at `key` (of the job table or the allocation table) calls for:
/// one `unread` entry for a row this build cannot read.
fn row_entries(key: &str, value: &[u8]) -> BTreeSet<String> {
    let read = if key.starts_with(keys::JOB) {
        job_at(key, value).map(|job| job_entries(&job))
    } else {
        allocation_at(key, value).map(|allocation| allocation_entries(&allocation))
    };
    read.unwrap_or_else(|_| keys::unread(key).into_iter().collect())
}

/// One row's fix of a backfill: the entries to move from `before` to `after`, on `cond`, the
/// row's version as read (or absent).
struct Fix {
    row: String,
    cond: Cond,
    before: BTreeSet<String>,
    after: BTreeSet<String>,
}

impl Fix {
    /// The row's transaction: the check of the row on `cond`, and the entries moved.
    fn txn(&self) -> Txn {
        moved(
            Txn::new().check(self.row.as_str(), self.cond),
            &self.before,
            &self.after,
        )
    }
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
    pub(super) fn entries(&self, span: &Span<'_>) -> Result<Vec<String>, JobStoreError> {
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
    /// allocation row, then every entry, and for each row whose entries differ from those it
    /// calls for, deletes the entries it does not call for and puts those it does, in a
    /// transaction of that row's own with the row's version as its precondition (absent for
    /// entries whose row is gone), many rows to a store batch. A row this build cannot read
    /// calls for one `unread` entry, which every indexed read of its table reads, so it is
    /// judged as a scan would judge it. An entry of a space this build does not name is left as
    /// it is. A second backfill with nothing written in between changes nothing and commits
    /// nothing.
    ///
    /// Another writer may change rows while it runs (the serving server of a start-first
    /// update). The rows are read before the entries, so a transaction that holds found the row
    /// unchanged since before the entries were read: its entries were what it judged. One
    /// whose row changed since loses alone, and that row is read again and decided again: its
    /// transaction deletes every entry the backfill knows of that the row now does not call
    /// for, and puts every one it does, at the version read again, until it holds or the row
    /// does not need it. Nothing starts over: the tables and the spaces are read once, and
    /// each row is read again only as often as another writer changed it in between. When
    /// rows were read again it logs one line with their count.
    ///
    /// [`JobStore::new`] runs it, so every job store opened, a server's at its start included,
    /// reads indexes that agree with its tables.
    ///
    /// # Errors
    /// [`JobStoreError::Store`]; [`JobStoreError::NodeMoved`] on a guarded view whose node
    /// row moved.
    pub fn backfill_indexes(&self) -> Result<Backfill, JobStoreError> {
        let wanted = self.wanted_entries()?;
        let stored = self.stored_entries()?;
        let rows: BTreeSet<&String> = wanted.keys().chain(stored.keys()).collect();
        let none = BTreeSet::new();
        let mut due = Vec::new();
        for row in rows {
            let (cond, want) = match wanted.get(row) {
                Some((version, entries)) => (Cond::Version(*version), entries.clone()),
                None => (Cond::Absent, BTreeSet::new()),
            };
            let have = stored.get(row).unwrap_or(&none);
            if &want != have {
                due.push(Fix {
                    row: row.clone(),
                    cond,
                    before: have.clone(),
                    after: want,
                });
            }
        }
        let mut done = Backfill::default();
        let mut again: usize = 0;
        while !due.is_empty() {
            let held = self.write_each(due.iter().map(Fix::txn).collect())?;
            let mut lost = Vec::new();
            for (fix, held) in due.into_iter().zip(held) {
                if held {
                    done.added += fix.after.difference(&fix.before).count();
                    done.removed += fix.before.difference(&fix.after).count();
                } else {
                    lost.push(fix);
                }
            }
            again += lost.len();
            due = Vec::with_capacity(lost.len());
            for fix in lost {
                due.push(self.fix_again(fix)?);
            }
        }
        said_again(again);
        Ok(done)
    }

    /// The fix of the row of `fix`, whose transaction lost, decided again on the row as it is
    /// now: every entry the backfill knows of that the row does not call for deleted and every
    /// one it calls for put, at the version read now (absent for a row gone).
    fn fix_again(&self, fix: Fix) -> Result<Fix, JobStoreError> {
        let row = self.store.get(&fix.row)?;
        let (cond, after) = match &row {
            Some(row) => (
                Cond::Version(row.version),
                row_entries(&fix.row, &row.value),
            ),
            None => (Cond::Absent, BTreeSet::new()),
        };
        let known = fix.before.union(&fix.after);
        let before: BTreeSet<String> = known
            .filter(|entry| !after.contains(*entry))
            .cloned()
            .collect();
        Ok(Fix {
            row: fix.row,
            cond,
            before,
            after,
        })
    }

    /// Every job and allocation row with its version and the entries it calls for, read by a
    /// scan of both tables: the reference the index spaces must agree with.
    fn wanted_entries(
        &self,
    ) -> Result<BTreeMap<String, (Version, BTreeSet<String>)>, JobStoreError> {
        let mut wanted = BTreeMap::new();
        let mut rows = self.store.scan_prefix(keys::JOB)?;
        rows.extend(self.store.scan_prefix(keys::ALLOCATION)?);
        for (key, row) in rows {
            let entries = row_entries(&key, &row.value);
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
        let wanted = self.wanted_entries();
        wanted.map(|rows| {
            rows.into_values()
                .flat_map(|(_, entries)| entries)
                .collect()
        })
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
