//! [`Written`]: where a job store's writes, and the store's committed head, stood when it was
//! asked ([`JobStore::written`]), so a caller that read the tables can tell that they have not
//! changed since without reading them again.

use std::sync::atomic::Ordering;

use js_state::{Revision, Store};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::IdSource;

/// Where the writes of one job store, and the store under it, stood when it was asked
/// ([`JobStore::written`]): two that are equal were taken from the same job store with no write
/// of it in between and, when the store tells its head, with no commit to the store by anyone
/// in between, so its tables held the same rows at both. Taken from two job stores, they are
/// never equal.
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
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read.
    pub fn written(&self) -> Result<Written, JobStoreError> {
        Ok(Written {
            instance: self.instance,
            writes: self.writes.load(Ordering::SeqCst),
            head: self.store.head()?,
        })
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
