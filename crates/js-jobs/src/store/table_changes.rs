//! What changed in the tables a pass over the queued jobs reads ([`JobStore::table_changes`]):
//! the queued jobs, the live allocations, the marks and the cordons, read from the store's feed
//! rather than from the tables, so a reader that holds the tables as they stood at one revision
//! brings them up to date by the rows that changed since, and not by reading every queued job
//! again. The feed's rows carry the values written, so a job or an allocation that changed is
//! decoded from its last row there, as [`JobStore::queued`] and [`JobStore::live_allocations`]
//! judge the row; for the marks and the cordons, a small table each, the answer only says that
//! one changed. A row that cannot be decoded is not judged from the feed: the tables are then
//! read in full, which fails only if it reads that row.

use std::collections::BTreeMap;

use js_state::{Change, Event, Revision, Store};

use super::{JobStore, allocation_at, same_key};
use crate::codec::{DecodeError, decode_job};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, JobRecord, JobState};

/// The most rows of the feed one call reads: past that many changes since the revision asked
/// after, the call answers that it cannot tell, and reading the tables in full costs about as
/// much as reading the feed on.
pub const MOST_TABLE_CHANGES: usize = 4096;

/// How many rows of the feed one read of it asks for.
const PAGE: usize = 512;

/// The rows of the tables a pass over the queued jobs reads that changed after a revision, each
/// as the last of its rows the feed gave ([`JobStore::table_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableChanges {
    /// How far the feed was read: every change at or below it that is not told here was told
    /// by an earlier answer, so the next call asks after it.
    pub head: Revision,
    /// Each job whose row changed: `Some` with its record when the job is queued; `None` when
    /// it is not, when its row was deleted, or when a later build wrote it (logged), which
    /// [`JobStore::queued`] leaves out too.
    pub queued: BTreeMap<JobId, Option<JobRecord>>,
    /// Each job whose allocation's row changed: `Some` with the allocation when it is live
    /// (booked or releasing), `None` when it was freed or its row deleted.
    pub live: BTreeMap<JobId, Option<AllocationRecord>>,
    /// Whether a mark's row was written, deleted or expired.
    pub marks: bool,
    /// Whether a cordon's row was written, deleted or lapsed.
    pub cordons: bool,
}

impl TableChanges {
    /// Notes `change`, a row of the feed, over what earlier rows noted: whether it could be
    /// judged from the feed. One cannot when it is an entry of the rows this build cannot
    /// read, a job or allocation key that names no job, a job row that does not parse or holds
    /// another job, or an allocation row that does not parse or that a later build wrote. A row
    /// of no table the pass reads is judged, and changes nothing.
    fn note(&mut self, change: Change) -> bool {
        let Change { key, event, .. } = change;
        if key.starts_with(keys::UNREAD_JOB) || key.starts_with(keys::UNREAD_ALLOCATION) {
            return false;
        }
        if let Some(id) = key.strip_prefix(keys::JOB) {
            let Ok(id) = JobId::parse(id) else {
                return false;
            };
            let job = match event {
                Event::Put(value) => match queued_at(&key, &value) {
                    Some(job) => job,
                    None => return false,
                },
                Event::Delete | Event::Expire => None,
            };
            self.queued.insert(id, job);
            return true;
        }
        if let Some(id) = key.strip_prefix(keys::ALLOCATION) {
            let Ok(id) = JobId::parse(id) else {
                return false;
            };
            let allocation = match event {
                Event::Put(value) => match allocation_at(&key, &value) {
                    Ok(allocation) => Some(allocation),
                    Err(_) => return false,
                },
                Event::Delete | Event::Expire => None,
            };
            let live = allocation.filter(|allocation| allocation.step != AllocationStep::Freed);
            self.live.insert(id, live);
            return true;
        }
        if key.starts_with(keys::MARK) {
            self.marks = true;
            return true;
        }
        if key.starts_with(keys::CORDON) {
            self.cordons = true;
            return true;
        }
        true
    }
}

/// The job the row `key` holds when it is queued, judged as [`JobStore::queued`] judges it
/// (`Some(None)` for a job that is not queued): a row a later build wrote is left out and
/// logged. `None` for a row that does not parse or holds another job.
fn queued_at(key: &str, value: &[u8]) -> Option<Option<JobRecord>> {
    let own = |job: JobRecord| same_key(key, keys::job(&job.id), job);
    match decode_job(value).and_then(own) {
        Ok(job) => Some((job.state == JobState::Queued).then_some(job)),
        Err(DecodeError::Newer) => {
            tracing::warn!(%key, "a job row a later build wrote is left out of the queue");
            Some(None)
        }
        Err(_) => None,
    }
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// The store's committed head ([`Store::head`]): a reader that reads the tables in full
    /// after taking it reads every commit at or below it, so it follows the tables from it
    /// with [`JobStore::table_changes`]. `None` for a store that does not tell its head.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the head cannot be read.
    pub fn feed_head(&self) -> Result<Option<Revision>, JobStoreError> {
        Ok(self.store.head()?)
    }

    /// The rows of the queued jobs, the live allocations, the marks and the cordons that
    /// changed after `after`, read from the store's feed up to the head the store tells when
    /// the call begins (or up to where the feed stops short of it, a commit still to land):
    /// each job and allocation as its last row there gives it, so tables read at `after` with
    /// these applied are the tables at [`TableChanges::head`]. Rows of every other key (the
    /// hosts' node rows, the duties, the dispatch messages, the index entries) are passed over.
    ///
    /// `None` when the feed cannot tell, and the tables are then to be read in full: the store
    /// tells no head, the feed was trimmed past `after` or cannot be read, more than
    /// [`MOST_TABLE_CHANGES`] rows came since, or a row is an entry of the rows this build
    /// cannot read, names no job, or cannot be decoded (the read in full then fails only if it
    /// reads that row).
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the head cannot be read.
    pub fn table_changes(&self, after: Revision) -> Result<Option<TableChanges>, JobStoreError> {
        changes_after(&*self.store, after)
    }
}

/// [`JobStore::table_changes`] of `store`, one copy for every kind of store.
fn changes_after(
    store: &dyn Store,
    after: Revision,
) -> Result<Option<TableChanges>, JobStoreError> {
    let Some(head) = store.head()? else {
        return Ok(None);
    };
    let mut changes = TableChanges {
        head: after,
        queued: BTreeMap::new(),
        live: BTreeMap::new(),
        marks: false,
        cordons: false,
    };
    let mut read = 0usize;
    while changes.head < head {
        let Ok(page) = store.changes(changes.head, PAGE) else {
            return Ok(None);
        };
        read = read.saturating_add(page.rows.len());
        if read > MOST_TABLE_CHANGES {
            return Ok(None);
        }
        for change in page.rows {
            if !changes.note(change) {
                return Ok(None);
            }
        }
        // A feed that stops short of the head (a commit below it still to land) is read on
        // from where it stopped by the next call.
        if page.cursor <= changes.head {
            break;
        }
        changes.head = page.cursor;
    }
    Ok(Some(changes))
}

#[cfg(test)]
#[path = "table_changes_tests.rs"]
mod tests;
