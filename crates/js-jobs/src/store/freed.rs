//! The runs of the freed allocations, newest free first: what a server teaches the history of
//! its job sizes from. A page walks the freed entries (`idx/alloc/freed/<at>/<id>`) in reverse,
//! so its first page holds the runs freed last, and reads the allocation rows and then the job
//! rows its entries point at [`SWEEP_BATCH`] keys to a read ([`Store::get_many`]), never a row
//! at a time.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use js_state::{ResumeToken, ScanRange, Store};

use super::{JobStore, MAX_PAGE, allocation_at, job_at};
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;
use crate::record::{AllocationStep, Booked, JobRecord};
use crate::result::SWEEP_BATCH;

/// A page of the runs of the freed allocations, newest free first ([`JobStore::freed_page`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreedPage {
    /// Each job of the page that ended with a result, with what its freed allocation booked, in
    /// the order of the job ids (a reader that needs them in another order sorts them).
    pub runs: Vec<(JobRecord, Booked)>,
    /// How many freed entries the page walked: a page can hold fewer runs, since an entry's
    /// job row may be gone (a job's row is kept for less time than its freed allocation's).
    pub entries: usize,
    /// Where the next page begins, toward older frees; `None` after the oldest.
    pub resume: Option<ResumeToken>,
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// A page of at most `limit` (and at most [`MAX_PAGE`]) freed entries, newest free first,
    /// from the newest or from `resume`, with the run each points at: its job, when its row is
    /// still held and has a result, with what its allocation booked. An entry whose allocation
    /// or job row is gone, or is not freed, is passed over, and a row a later build wrote is
    /// passed over and logged. It reads one page of entries and then the allocation and the job
    /// rows [`SWEEP_BATCH`] keys to a read.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row does not parse; [`JobStoreError::Store`].
    pub fn freed_page(
        &self,
        resume: Option<&ResumeToken>,
        limit: NonZeroUsize,
    ) -> Result<FreedPage, JobStoreError> {
        let scan = ScanRange::new(keys::FREED, limit.min(MAX_PAGE)).reverse();
        let scan = match resume {
            Some(token) => scan.resume(token),
            None => scan,
        };
        let page = self.store.scan_range(scan)?;
        let entries = page.rows.len();
        let mut booked: BTreeMap<JobId, Booked> = BTreeMap::new();
        let allocations = self.rows_of(page.rows.into_iter().map(|(key, _)| key))?;
        for (key, row) in allocations {
            let allocation = readable(&key, allocation_at(&key, &row.value))?;
            let freed = allocation.filter(|allocation| allocation.step == AllocationStep::Freed);
            booked.extend(freed.map(|allocation| (allocation.job, allocation.booked)));
        }
        let job_keys: Vec<String> = booked.keys().map(keys::job).collect();
        let mut runs = Vec::with_capacity(job_keys.len());
        for batch in job_keys.chunks(SWEEP_BATCH.get()) {
            for (key, row) in self.store.get_many(batch)? {
                let job = readable(&key, job_at(&key, &row.value))?;
                let ended = job.filter(|job| job.result.is_some());
                let run = ended.and_then(|job| booked.get(&job.id).map(|share| (job, *share)));
                runs.extend(run);
            }
        }
        Ok(FreedPage {
            runs,
            entries,
            resume: page.resume,
        })
    }
}

/// `read`, the record of the row `key`, or `None` for a row a later build wrote, which is
/// logged and passed over; any other error is the read's.
fn readable<T>(key: &str, read: Result<T, JobStoreError>) -> Result<Option<T>, JobStoreError> {
    match read {
        Ok(record) => Ok(Some(record)),
        Err(JobStoreError::UnknownRow { .. }) => {
            tracing::warn!(%key, "a row a later build wrote is left out of the freed runs");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "freed_tests.rs"]
mod tests;
