//! Helpers the test files share.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{
    Changes, Clock, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage,
    ScanRange, Store, StoreError, StoreTime, Txn, Versioned,
};
use js_testkit::dir::TestDir;
use js_types::{HostId, LeaseId};
use js_usage::NodeMinute;

pub fn host(name: &str) -> HostId {
    HostId::new(name).expect("a valid host id")
}

pub fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId::new(term, seq)
}

/// A node minute whose three figures all differ, so a swapped field shows.
pub fn row(minute: u64) -> NodeMinute {
    NodeMinute {
        minute,
        cpu_millis: u32::try_from(minute % 100_000).unwrap_or(0) + 1,
        ws_max_bytes: minute * 1000 + 7,
        leases: u32::try_from(minute % 50).unwrap_or(0) + 2,
    }
}

/// Runs `test` over the in-memory fake and then over a real SQLite file in a scratch
/// directory, each with a fresh, empty store.
pub fn on_both(name: &str, test: impl Fn(&dyn Store)) {
    let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    test(&fake);
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-usage-{name}"));
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
    let sqlite = SqliteStore::open(&dir.path().join("state.db"), clock, Options::default())
        .expect("open the scratch store");
    test(&sqlite);
}

/// A fresh fake store.
pub fn fake() -> Fake {
    Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None)
}

/// Puts `value` under `key` unconditionally.
pub fn put(store: &dyn Store, key: &str, value: &[u8]) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// A store that delegates to another and can be told to fail a call, or to let a second
/// writer in right after a read.
pub struct Wrapped<'a> {
    pub inner: &'a dyn Store,
    pub fail_get: AtomicBool,
    pub fail_scan: AtomicBool,
    pub fail_batch: AtomicBool,
    /// After the next `get`, write this (key, value) through the inner store.
    pub after_get: Mutex<Option<(String, Vec<u8>)>>,
    /// After the next `scan_range`, write this (key, value) through the inner store.
    pub after_scan: Mutex<Option<(String, Vec<u8>)>>,
}

impl<'a> Wrapped<'a> {
    pub fn new(inner: &'a dyn Store) -> Self {
        Self {
            inner,
            fail_get: AtomicBool::new(false),
            fail_scan: AtomicBool::new(false),
            fail_batch: AtomicBool::new(false),
            after_get: Mutex::new(None),
            after_scan: Mutex::new(None),
        }
    }
}

fn boom() -> StoreError {
    StoreError::Failed("boom".to_owned())
}

fn interfere(inner: &dyn Store, slot: &Mutex<Option<(String, Vec<u8>)>>) {
    if let Some((key, value)) = slot.lock().expect("lock").take() {
        put(inner, &key, &value);
    }
}

impl Store for Wrapped<'_> {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fail_batch.load(Ordering::SeqCst) {
            return Err(boom());
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(boom());
        }
        let found = self.inner.get(key)?;
        interfere(self.inner, &self.after_get);
        Ok(found)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        if self.fail_scan.load(Ordering::SeqCst) {
            return Err(boom());
        }
        let page = self.inner.scan_range(scan)?;
        interfere(self.inner, &self.after_scan);
        Ok(page)
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
