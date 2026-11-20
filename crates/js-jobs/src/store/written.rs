//! [`Written`]: where a job store's writes, and the store's committed head, stood when it was
//! asked ([`JobStore::written`]), so a caller that read the tables can tell that they have not
//! changed since without reading them again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use js_state::{Revision, Store, Version};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::IdSource;

/// Where the writes of one job store, and the store under it, stood when it was asked
/// ([`JobStore::written`]): two that are equal were taken from the same job store with no write
/// of it in between and, when the store tells its head, with no commit to the store by anyone
/// in between, but one that only refreshed when a host was heard ([`JobStore::refresh_node`]),
/// so its tables held the same rows at both, but for that hearing. Taken from two job stores,
/// they are never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The job store's number among those this process made.
    instance: u64,
    /// How many writes it had sent to its store.
    writes: u64,
    /// The store's committed head revision, when it tells one ([`Store::head`]): what another
    /// server's commit moves.
    head: Option<Revision>,
}

/// What a job store counts of its writes: how many it sent, and the last commit it made that
/// only refreshed when a host was heard, with the reading it leaves standing.
#[derive(Debug, Default)]
pub(super) struct Writes {
    sent: AtomicU64,
    heard: Mutex<Option<Heard>>,
}

/// A commit that only refreshed when a host was heard: the head (for a store that tells it) and
/// the count of writes sent just after it, and the reading of [`JobStore::written`] from just
/// before it.
#[derive(Debug, Clone, Copy)]
struct Heard {
    head: Option<Revision>,
    sent: u64,
    before: Written,
}

impl Writes {
    /// Counts `n` more writes sent, as [`AtomicU64::fetch_add`].
    pub(super) fn fetch_add(&self, n: u64, order: Ordering) -> u64 {
        self.sent.fetch_add(n, order)
    }

    fn heard(&self) -> std::sync::MutexGuard<'_, Option<Heard>> {
        self.heard.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Where this job store's writes stand, and where the store's head stands: each transaction
    /// it sends counts once, after the store answered it, whether it committed, conflicted or
    /// failed (a failed commit may still have landed), and the store's committed head moves with
    /// every commit, this job store's or another server's. The tables change only through such
    /// a commit, so when two readings are equal the tables held the same rows at both. A store
    /// that does not tell its head ([`Store::head`]) is one only this process writes, and its
    /// own writes are the count. The write count is kept in memory and starts again with each job
    /// store; the head is the store's, one small read.
    ///
    /// One commit is left out: a refresh of when a host was heard, and of nothing else, that
    /// this job store committed directly on the head ([`JobStore::refresh_node`]), with no commit
    /// or write in between. Until the next commit or write the reading is the one from before it,
    /// so a caller's copy of the tables stands; a copy of the `node/` rows then holds that host's
    /// earlier hearing, which only makes the host look heard less lately than it was.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read.
    pub fn written(&self) -> Result<Written, JobStoreError> {
        Ok(self.readings()?.1)
    }

    /// Where this job store's writes and the store's head stand, and what [`JobStore::written`]
    /// answers for it.
    fn readings(&self) -> Result<(Written, Written), JobStoreError> {
        let now = Written {
            instance: self.instance,
            writes: self.writes.sent.load(Ordering::SeqCst),
            head: self.store.head()?,
        };
        let shown = match *self.writes.heard() {
            Some(heard) if (now.head == heard.head) & (now.writes == heard.sent) => heard.before,
            _ => now,
        };
        Ok((now, shown))
    }

    /// Runs `refresh`, a commit that only refreshes when a host was heard and answers the
    /// version of the row it wrote (`None` when it wrote nothing), and leaves it out of
    /// [`JobStore::written`] when it was the one write this job store sent since the reading
    /// before it and, for a store that tells its head, it committed directly on that head: the
    /// row's version, which is the commit's revision, is the head's next.
    pub(super) fn hearing<T>(
        &self,
        refresh: impl FnOnce() -> Result<(T, Option<Version>), JobStoreError>,
    ) -> Result<T, JobStoreError> {
        let (before, shown) = self.readings()?;
        let (answer, wrote) = refresh()?;
        let sent = self.writes.sent.load(Ordering::SeqCst);
        let alone = sent == before.writes.saturating_add(1);
        let on_head = |version: &Version| {
            before
                .head
                .is_none_or(|head| head.0.checked_add(1) == Some(version.0))
        };
        if let Some(version) = wrote.filter(on_head).filter(|_| alone) {
            let heard = Heard {
                head: before.head.map(|_| Revision(version.0)),
                sent,
                before: shown,
            };
            *self.writes.heard() = Some(heard);
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::fake::{Fake, Flaw};
    use js_state::{ManualClock, StoreError};

    use super::JobStore;
    use crate::error::JobStoreError;
    use crate::ids::RandomIds;
    use crate::interfere::Interfere;

    // Catches a reading that answers a mark without the store's head when the head cannot be
    // read, or answers some error other than the store's.
    #[test]
    fn a_head_that_cannot_be_read_fails_the_reading_with_the_stores_error() {
        let store = Arc::new(Interfere::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        let before = jobs.written().expect("the head reads");
        store.fail_with(StoreError::Disconnected("the head read failed".to_owned()));
        store.fail_next_head();
        assert_eq!(
            jobs.written(),
            Err(JobStoreError::Store(StoreError::Disconnected(
                "the head read failed".to_owned()
            )))
        );
        assert_eq!(
            jobs.written(),
            Ok(before),
            "only the one head read fails, and nothing was written in between"
        );
    }
}
