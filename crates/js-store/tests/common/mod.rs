//! Helpers the test files share: a store that lets another frontend's write land in the
//! middle of one of this frontend's, and a handle over a store.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, Rows, StateError};

/// A store that applies a transaction of another frontend just before a write of this one
/// that names the transaction's key, oldest first: a write landing between this
/// frontend's read of a row and its own write of it. It can also refuse to read one row.
pub struct Racing {
    inner: Fake,
    clock: Arc<ManualClock>,
    landings: Mutex<Vec<(String, Txn)>>,
    down: AtomicBool,
    unreadable: Mutex<Option<String>>,
    ack_lost: AtomicBool,
}

impl Racing {
    pub fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::default());
        Arc::new(Self {
            inner: Fake::with_flaw(clock.clone(), Flaw::None),
            clock,
            landings: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            unreadable: Mutex::new(None),
            ack_lost: AtomicBool::new(false),
        })
    }

    /// The next write is applied and then reported as failed, as a connection lost after the
    /// commit reports it: the rows are in, and nobody knows.
    pub fn lose_next_ack(&self) {
        self.ack_lost.store(true, Ordering::SeqCst);
    }

    /// The store's clock moves on by `by`.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// `landing` is applied before the next write that names `key`.
    pub fn land(&self, key: &str, landing: Txn) {
        let mut queued = self.landings.lock().expect("lock");
        queued.push((key.to_owned(), landing));
    }

    /// The store takes writes again, or not.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// A read of the row `key` fails from now on, or (`None`) reads again.
    pub fn set_unreadable(&self, key: Option<&str>) {
        *self.unreadable.lock().expect("lock") = key.map(str::to_owned);
    }
}

impl Store for Racing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        let names = |key: &str| {
            let mut ops = batch.iter().flat_map(Txn::ops);
            ops.any(|op| op.key_cond().is_some_and(|(named, _)| named == key))
        };
        let mut queued = self.landings.lock().expect("lock");
        if let Some(at) = queued.iter().position(|(key, _)| names(key)) {
            let (_, landing) = queued.remove(at);
            self.inner.txn(landing)?.expect("the landing write");
        }
        drop(queued);
        let applied = self.inner.apply_batch(batch)?;
        if self.ack_lost.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Failed("the connection closed".to_owned()));
        }
        Ok(applied)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.unreadable.lock().expect("lock").as_deref() == Some(key) {
            return Err(StoreError::Failed(format!("{key} cannot be read")));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// [`Rows::open`] over `store`, with a codec of its own.
pub fn open(
    store: Arc<Racing>,
    policy: EpochPolicy,
    first: u64,
) -> Result<(Rows, u64), StateError> {
    Rows::open(
        store,
        policy,
        first,
        Arc::new(TestCodec::new()),
        Arc::new(TestExt),
    )
}

/// A handle over `store` for the frontend numbered `id`, which kept the epoch it found or
/// made (`id` in an empty store), encoding row values with `codec`.
pub fn frontend_with(store: &Arc<Racing>, id: u64, codec: &Arc<TestCodec>) -> Rows {
    let store: Arc<dyn Store> = store.clone();
    let codec = codec.clone();
    Rows::open(store, EpochPolicy::Keep, id, codec, Arc::new(TestExt))
        .expect("open")
        .0
}
