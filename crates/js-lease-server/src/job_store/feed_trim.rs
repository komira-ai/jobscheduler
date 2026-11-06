//! Trimming the job store's change feed: the feed grows by a row for every change of the
//! job tables, and the tick keeps only the newest `--job-feed-retention` of them.
//!
//! The cutoff comes from the feed's own sequence, never from a clock: the store's head is
//! read with a transaction that writes nothing (its commit names the head, see
//! [`js_state::Commit::revision`]), and everything at or below the head less the count to
//! keep is trimmed ([`js_state::Store::trim_feed`]). A reader that was behind the cutoff
//! is answered [`js_state::StoreError::StaleCursor`] and starts again from the tables.

use std::sync::Arc;

use js_state::{Revision, Store, StoreError, Txn};

use crate::config::FeedRetention;

/// The job store as the tick trims it, and how much of its feed to keep.
pub(crate) struct FeedTrim {
    store: Arc<dyn Store>,
    keep: FeedRetention,
}

impl FeedTrim {
    /// The feed of `store`, kept to its newest `keep` changes.
    pub(crate) fn new(store: Arc<dyn Store>, keep: FeedRetention) -> Self {
        Self { store, keep }
    }

    /// Trims the feed once ([`trim`]); how many rows it deleted.
    ///
    /// # Errors
    /// [`StoreError`] when the store fails.
    pub(crate) fn pass(&self) -> Result<u64, StoreError> {
        trim(self.store.as_ref(), self.keep)
    }
}

/// Deletes every feed row of `store` but the newest `keep`; how many it deleted. A store
/// whose head is not past `keep` is not touched. (A transaction that writes nothing cannot
/// fail its preconditions; were a store to say it had, the head is read as 0 and nothing
/// is trimmed.)
///
/// # Errors
/// [`StoreError`] when the store fails.
pub(crate) fn trim(store: &dyn Store, keep: FeedRetention) -> Result<u64, StoreError> {
    let head = store
        .txn(Txn::new())?
        .map_or(Revision::ZERO, |commit| commit.revision);
    match head.0.saturating_sub(keep.keep()) {
        0 => Ok(0),
        cutoff => store.trim_feed(Revision(cutoff)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_jobs::interfere::{INJECTED, Interfere};
    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Revision, Store, StoreError, Txn};

    use super::{FeedTrim, trim};
    use crate::config::FeedRetention;

    fn fake() -> Fake {
        Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None)
    }

    fn put_all(store: &Fake, count: u64) {
        for n in 1..=count {
            store
                .txn(Txn::new().put(format!("k/{n}"), "v", Cond::Any))
                .unwrap()
                .unwrap();
        }
    }

    fn keep(count: u64) -> FeedRetention {
        FeedRetention::new(count).unwrap()
    }

    fn revisions(store: &Fake, from: u64) -> Vec<u64> {
        let page = store.changes(Revision(from), 1000).unwrap();
        page.rows.iter().map(|row| row.revision.0).collect()
    }

    // Catches a trim that keeps more or fewer than the newest N, or counts wrongly: of ten
    // changes keeping four leaves exactly revisions 7 to 10 and deletes six.
    #[test]
    fn the_newest_n_changes_are_kept() {
        let store = fake();
        put_all(&store, 10);
        assert_eq!(trim(&store, keep(4)), Ok(6));
        assert_eq!(revisions(&store, 6), [7, 8, 9, 10]);
        assert_eq!(
            store.changes(Revision(5), 10),
            Err(StoreError::StaleCursor {
                after: Revision(5),
                floor: Revision(6)
            })
        );
        assert_eq!(
            trim(&store, keep(4)),
            Ok(0),
            "a second pass finds nothing more"
        );
    }

    // Catches a trim that touches a feed with no more than N changes, whatever the count
    // is: the feed is then served from the start.
    #[test]
    fn a_feed_with_no_more_than_n_changes_is_not_touched() {
        let store = fake();
        put_all(&store, 3);
        assert_eq!(trim(&store, keep(3)), Ok(0));
        assert_eq!(trim(&store, keep(100_000)), Ok(0));
        assert_eq!(revisions(&store, 0), [1, 2, 3]);
        let empty = fake();
        assert_eq!(trim(&empty, keep(1)), Ok(0));
    }

    // Catches a cutoff that does not follow the head: after more changes the next pass trims
    // the rows that fell out of the window and no others, through the handle the tick holds.
    #[test]
    fn the_window_follows_the_head() {
        let store = Arc::new(fake());
        let feed = FeedTrim::new(store.clone(), keep(2));
        put_all(&store, 5);
        assert_eq!(feed.pass(), Ok(3));
        put_all(&store, 2);
        assert_eq!(feed.pass(), Ok(2));
        assert_eq!(revisions(&store, 5), [6, 7]);
    }

    // Catches an error swallowed or rewritten: a store that fails the transaction that reads
    // its head is returned as the store said it, and nothing is trimmed.
    #[test]
    fn a_failing_stores_error_is_returned() {
        let store = Interfere::new(fake());
        put_all(&store.inner(), 5);
        store.fail_next();
        assert_eq!(
            trim(&store, keep(1)),
            Err(StoreError::Failed(INJECTED.to_owned()))
        );
        assert_eq!(revisions(&store.inner(), 0), [1, 2, 3, 4, 5]);
        assert_eq!(trim(&store, keep(1)), Ok(4), "the next call trims");
    }
}
