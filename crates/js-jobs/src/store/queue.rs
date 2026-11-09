//! What the placement engine reads of the tables on a pass (design sections 5.3, step 5,
//! and 6.2): the queued jobs in queue order ([`JobStore::queued`]), and every live
//! allocation ([`JobStore::live_allocations`]).

use std::cmp::Ordering;

use js_state::Store;

use super::index::Span;
use super::{JobStore, allocation_at, same_key};
use crate::codec::{DecodeError, decode_job};
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;
use crate::record::{AllocationRecord, AllocationStep, JobRecord, JobState};

/// The queue order of two jobs: the higher priority first, then the one submitted first (by
/// the store's clock), then the lower job id, so no two jobs tie.
#[must_use]
pub fn queue_order(a: &JobRecord, b: &JobRecord) -> Ordering {
    b.priority
        .cmp(&a.priority)
        .then(a.submitted_at.cmp(&b.submitted_at))
        .then(a.id.cmp(&b.id))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Every queued job, in queue order ([`queue_order`]). It reads the rows of the queued
    /// jobs, through their `queued` entries, and the rows this build cannot read; the row is
    /// judged, not the entry, so a row an entry names that is no longer queued is left out. A
    /// row a later build wrote is left out and logged, as [`JobStore::list_jobs`] leaves it out.
    ///
    /// # Errors
    /// [`JobStoreError::Corrupt`] when a row it reads does not parse; [`JobStoreError::Store`].
    pub fn queued(&self) -> Result<Vec<JobRecord>, JobStoreError> {
        let mut queued = Vec::new();
        let spans = [Span::all(keys::QUEUED), Span::all(keys::UNREAD_JOB)];
        for (key, row) in self.indexed_rows(&spans)? {
            match decode_job(&row.value).and_then(|job| same_key(&key, keys::job(&job.id), job)) {
                Ok(job) => queued.extend((job.state == JobState::Queued).then_some(job)),
                Err(DecodeError::Newer) => {
                    tracing::warn!(%key, "a job row a later build wrote is left out of the queue");
                }
                Err(why) => return Err(JobStoreError::Corrupt { key, why }),
            }
        }
        queued.sort_by(queue_order);
        Ok(queued)
    }

    /// Every live (booked or releasing) allocation, of every host, in job id order. It reads
    /// the rows of the live allocations, through their on-host entries, and the allocation
    /// rows this build cannot read; no freed row.
    ///
    /// # Errors
    /// [`JobStoreError::UnknownRow`] or [`JobStoreError::Corrupt`] when an allocation row
    /// cannot be read (a booking that cannot be counted is not taken as none);
    /// [`JobStoreError::Store`].
    pub fn live_allocations(&self) -> Result<Vec<AllocationRecord>, JobStoreError> {
        let mut live = Vec::new();
        let spans = [Span::all(keys::ON_HOST), Span::all(keys::UNREAD_ALLOCATION)];
        for (key, row) in self.indexed_rows(&spans)? {
            let allocation = allocation_at(&key, &row.value)?;
            live.extend((allocation.step != AllocationStep::Freed).then_some(allocation));
        }
        Ok(live)
    }
}
