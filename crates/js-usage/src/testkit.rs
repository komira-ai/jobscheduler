//! A state store for tests: a fake underneath, with failures to switch on, a hook that
//! runs after each read (to play a second writer), and a count of the transactions asked.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};

type Hook = Box<dyn FnMut(&Fake) + Send>;

pub(crate) struct TestStore {
    pub(crate) fake: Fake,
    pub(crate) fail_get: AtomicBool,
    pub(crate) fail_scan: AtomicBool,
    pub(crate) fail_txn: AtomicBool,
    /// Runs after every successful `get`, with the fake underneath.
    pub(crate) after_get: Mutex<Option<Hook>>,
    /// Transactions asked of the store (failed ones too).
    pub(crate) txns: AtomicUsize,
}

impl TestStore {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            fake: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            fail_get: AtomicBool::new(false),
            fail_scan: AtomicBool::new(false),
            fail_txn: AtomicBool::new(false),
            after_get: Mutex::new(None),
            txns: AtomicUsize::new(0),
        })
    }

    pub(crate) fn set(flag: &AtomicBool, on: bool) {
        flag.store(on, Ordering::SeqCst);
    }

    /// Runs `hook` after every later successful read.
    pub(crate) fn on_get(&self, hook: impl FnMut(&Fake) + Send + 'static) {
        *self
            .after_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    pub(crate) fn no_hook(&self) {
        *self
            .after_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn txn_count(&self) -> usize {
        self.txns.load(Ordering::SeqCst)
    }
}

fn down() -> StoreError {
    StoreError::Failed("down".to_owned())
}

impl Store for TestStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.txns.fetch_add(1, Ordering::SeqCst);
        if self.fail_txn.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.fake.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(down());
        }
        let got = self.fake.get(key)?;
        if let Some(hook) = self
            .after_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            hook(&self.fake);
        }
        Ok(got)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if self.fail_scan.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.fake.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.fake.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fake.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fake.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.fake.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.fake.now()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use js_state::{Cond, Revision};

    use super::*;

    /// Catches: the wrapper dropping a call it forwards, so a test over it reads a
    /// different store than the one written.
    #[test]
    fn the_wrapper_forwards_what_it_does_not_change() {
        let store = TestStore::new();
        store
            .txn(Txn::new().put("p/a", b"1".to_vec(), Cond::Any))
            .unwrap()
            .unwrap();
        let page = store
            .scan_range(ScanRange::new("p/", NonZeroUsize::MIN))
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            store.changes(Revision::default(), 10).unwrap().rows.len(),
            1
        );
        assert_eq!(store.reap_expired().unwrap(), 0);
        assert_eq!(store.commit_stats(), store.fake.commit_stats());
        assert_eq!(store.now(), store.fake.now());
    }
}
