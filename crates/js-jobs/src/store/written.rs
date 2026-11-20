//! [`Written`]: where a job store's writes of the job tables, and the store's committed head at
//! the last commit that changed a job table, stood when it was asked ([`JobStore::written`]), so
//! a caller that read the tables can tell that they have not changed since without reading them
//! again. A host's node row (`node/<host>`) is not a job table: every heartbeat writes its
//! host's row, and those writes alone leave a reading as it was.

use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use js_state::{Revision, Store};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;

/// How many rows of the store's feed one reading looks at past the head it last read: more
/// than that since then counts as a change of the tables.
const FEED_PAGE: usize = 512;

/// The store's head as the last reading read it, and the head at the last commit that changed
/// a job table as far as the readings since have told.
#[derive(Debug, Default)]
pub(super) struct Heads {
    /// The store's head at the last reading.
    seen: Option<Revision>,
    /// The head a reading gives: the store's head when the last change of a job table was
    /// seen.
    tables: Option<Revision>,
}

/// Where the writes of one job store to its job tables, and the store under it, stood when it was
/// asked ([`JobStore::written`]): two that are equal were taken from the same job store with no
/// write of its tables in between and, when the store tells its head, with no commit to a job
/// table by anyone in between (commits of node rows alone aside), so its tables held the same
/// rows at both. Taken from two job stores, they are never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The job store's number among those this process made.
    instance: u64,
    /// How many writes of its job tables it had sent to its store.
    writes: u64,
    /// The store's committed head revision at the last commit that changed a job table, as far
    /// as the feed told, when the store tells a head ([`Store::head`]): what another server's
    /// commit to a job table moves.
    head: Option<Revision>,
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Where this job store's writes of the job tables stand, and where the store's head stood at
    /// the last commit that changed a job table: each transaction it sends counts once, after
    /// the store answered it, whether it committed, conflicted or failed (a failed commit may
    /// still have landed), except a write of a host's node row alone; and the head is taken from
    /// the store's head, moved only when the feed's rows since the last reading hold a key that
    /// is not a node row ([`JobStore::tables_head`]). The tables change only through such a
    /// commit, so when two readings are equal the tables held the same rows at both, while the
    /// heartbeats' node writes leave a reading as it was. A store that does not tell its head
    /// ([`Store::head`]) is one only this process writes, and its own writes are the count. The
    /// write count is kept in memory and starts again with each job store; the head is one small
    /// read, and a page of the feed when it moved.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read.
    pub fn written(&self) -> Result<Written, JobStoreError> {
        Ok(Written {
            instance: self.instance,
            writes: self.writes.load(Ordering::SeqCst),
            head: self.tables_head()?,
        })
    }

    /// The store's head at the last commit that changed a job table, as far as the store's feed
    /// tells, or `None` for a store that tells no head. When the head moved since the last
    /// reading, the feed's rows since are read: a page whose every row is a node row leaves the
    /// answer as it was; a row of any other key, more rows than one page, or a feed that cannot
    /// be read (trimmed past the last reading) moves it to the head now. Only ever moved too
    /// often, never too seldom: a reader then reads the tables again for nothing.
    fn tables_head(&self) -> Result<Option<Revision>, JobStoreError> {
        let Some(head) = self.store.head()? else {
            return Ok(None);
        };
        let mut heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        let moved = match heads.seen {
            Some(seen) if seen == head => false,
            Some(seen) => self.store.changes(seen, FEED_PAGE).map_or(true, |page| {
                let tables = page.rows.iter().any(|row| !row.key.starts_with(keys::NODE));
                tables || page.cursor < head
            }),
            None => true,
        };
        if moved {
            heads.tables = Some(head);
        }
        heads.seen = Some(head);
        Ok(heads.tables)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Store, StoreError, Txn};

    use super::{FEED_PAGE, JobStore};
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

    /// A job store over a fake store that tells its head, and the store.
    fn tables() -> (Arc<Fake>, JobStore<Fake, RandomIds>) {
        let store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        (store, jobs)
    }

    /// Commits a put of each of `keys`, in one transaction.
    fn put(store: &Fake, keys: &[String]) {
        let txn = keys
            .iter()
            .fold(Txn::new(), |txn, key| txn.put(key.as_str(), "v", Cond::Any));
        store.txn(txn).expect("the store").expect("no conflict");
    }

    // Catches a reading that moves with every heartbeat (each writes its host's node row), so a
    // pass after a quiet beat reads the tables again and runs every round, and one that does not
    // move when a job table changes: node rows written directly or through the job store leave
    // the reading as it was, and a job row moves it.
    #[test]
    fn node_rows_leave_the_reading_and_a_job_table_moves_it() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        assert_eq!(jobs.written(), Ok(before), "nothing was written");
        put(&store, &["node/host-a".to_owned()]);
        assert_eq!(jobs.written(), Ok(before), "a node row moved the reading");
        let host = js_types::HostId::new("host-b").expect("an id");
        let hello = crate::NodeHello {
            session: None,
            pool: "pool".to_owned(),
            claim: None,
            shape: None,
            labels: std::collections::BTreeMap::new(),
            phase: js_agent_wire::Phase::Serving,
            agent_process: None,
        };
        jobs.hello_node(&host, &hello).expect("a hello");
        assert_eq!(
            jobs.written(),
            Ok(before),
            "the job store's node write moved it"
        );
        put(&store, &["node/host-c".to_owned(), "job/x".to_owned()]);
        let after = jobs.written().expect("reads");
        assert_ne!(after, before, "a job row did not move the reading");
        assert_eq!(jobs.written(), Ok(after));
    }

    // Catches a reading that judges only the first page of the feed: more node rows since the
    // last reading than one page holds moves it, since what lies past the page is not known.
    #[test]
    fn more_rows_than_a_page_move_the_reading() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        let keys: Vec<String> = (0..=FEED_PAGE).map(|n| format!("node/host-{n}")).collect();
        put(&store, &keys);
        assert_ne!(jobs.written(), Ok(before));
    }

    // Catches a reading that answers "unchanged" when the feed since the last reading cannot be
    // read (it was trimmed): a job table may have changed in the rows it lost.
    #[test]
    fn a_feed_trimmed_past_the_last_reading_moves_it() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        put(&store, &["node/host-a".to_owned()]);
        let head = store.head().expect("reads").expect("a head");
        store.trim_feed(head).expect("trims");
        assert_ne!(jobs.written(), Ok(before));
    }
}
