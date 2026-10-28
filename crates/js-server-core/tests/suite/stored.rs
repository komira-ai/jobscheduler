//! A state store a test can make fail, and the cores and handles over it: the farm's tests
//! of the rows over a store whose reads and writes fail on request, with the rig for an
//! adapter and the test codec for the rows' values.

#![allow(dead_code, reason = "each test file uses some of the setup")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_server_core::{Config, Core, Storage};
use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, Rows};
use js_types::sched::OperationId;

use crate::common::{Rig, at, begin};

/// A write another frontend makes, as a closure.
type Landing = Box<dyn FnOnce() + Send>;

/// The in-memory store, failing the reads and writes of the rows under a prefix on request.
pub struct Racing {
    inner: Fake,
    clock: Arc<ManualClock>,
    down: AtomicBool,
    fenced: AtomicBool,
    unreadable: Mutex<Option<String>>,
    unwritable: Mutex<Option<String>>,
    /// A write applied just before the next read of a row under a prefix: another frontend's
    /// write landing between two reads of one look.
    before_read: Mutex<Option<(String, Landing)>>,
    /// How many scans of each prefix were made.
    scans: Mutex<Vec<String>>,
}

impl Racing {
    pub fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::default());
        Arc::new(Self {
            inner: Fake::with_flaw(clock.clone(), Flaw::None),
            clock,
            down: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            unreadable: Mutex::new(None),
            unwritable: Mutex::new(None),
            before_read: Mutex::new(None),
            scans: Mutex::new(Vec::new()),
        })
    }

    /// Writes `value` at `key` as a writer of its own.
    pub fn put(&self, key: &str, value: Vec<u8>) {
        self.write(Txn::new().put(key, value, Cond::Any));
    }

    /// Deletes the row at `key` as a writer of its own.
    pub fn remove(&self, key: &str) {
        self.write(Txn::new().delete(key, Cond::Any));
    }

    /// Runs `landing` just before the next read of a row under `prefix` (once).
    pub fn before_read(&self, prefix: &str, landing: impl FnOnce() + Send + 'static) {
        let mut slot = self
            .before_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = Some((prefix.to_owned(), Box::new(landing)));
    }

    /// How many scans of the rows under `prefix` were made so far.
    pub fn scans_of(&self, prefix: &str) -> usize {
        let scans = self.scans.lock().unwrap_or_else(PoisonError::into_inner);
        scans
            .iter()
            .filter(|scanned| scanned.as_str() == prefix)
            .count()
    }

    fn landing(&self, key: &str) {
        let mut slot = self
            .before_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|(prefix, _)| key.starts_with(prefix.as_str()))
        {
            let (_, landing) = slot.take().expect("checked");
            drop(slot);
            landing();
        }
    }

    /// The store takes no writes (and says so as a store that cannot be reached does), or
    /// takes them again.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// The store says a newer writer holds it, to every write, or stops saying so.
    pub fn set_fenced(&self, fenced: bool) {
        self.fenced.store(fenced, Ordering::SeqCst);
    }

    /// A read of the rows under `prefix` fails from now on, or (`None`) reads again. `""`
    /// is every row.
    pub fn fail_reads(&self, prefix: Option<&str>) {
        *self
            .unreadable
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = prefix.map(str::to_owned);
    }

    /// A write of a row under `prefix` fails from now on, or (`None`) writes again.
    pub fn fail_writes(&self, prefix: Option<&str>) {
        *self
            .unwritable
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = prefix.map(str::to_owned);
    }

    /// The store's clock moves on by `by`.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The store itself, whatever it is made to fail.
    pub fn rows_of(&self, prefix: &str) -> Vec<(String, Versioned)> {
        self.inner.scan_prefix(prefix).expect("the fake scans")
    }

    /// Applies `txn` to the store as a writer of its own.
    pub fn write(&self, txn: Txn) {
        self.inner
            .txn(txn)
            .expect("the fake writes")
            .expect("no conflict");
    }

    fn reads_fail(&self, key: &str) -> bool {
        let unreadable = self
            .unreadable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        unreadable
            .as_deref()
            .is_some_and(|prefix| key.starts_with(prefix))
    }

    fn writes_fail(&self, batch: &[Txn]) -> bool {
        let unwritable = self
            .unwritable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(prefix) = unwritable.as_deref() else {
            return false;
        };
        batch.iter().flat_map(Txn::ops).any(|op| match op {
            Op::Put { key, .. } | Op::Delete { key, .. } => key.starts_with(prefix),
            Op::Check { .. } | Op::Next { .. } => false,
        })
    }
}

impl Store for Racing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fenced.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        if self.down.load(Ordering::SeqCst) || self.writes_fail(&batch) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.landing(key);
        if self.reads_fail(key) {
            return Err(StoreError::Failed(format!("{key} cannot be read")));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.scans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(prefix.to_owned());
        if self.reads_fail(prefix) {
            return Err(StoreError::Failed(format!("{prefix} cannot be read")));
        }
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

/// One store, and the codec the rows over it are encoded with: a row is read only by the
/// codec that wrote it, so every core and handle over the store shares it.
pub struct Setup {
    pub store: Arc<Racing>,
    pub codec: Arc<TestCodec>,
}

impl Setup {
    pub fn new() -> Self {
        begin();
        Self {
            store: Racing::new(),
            codec: Arc::new(TestCodec::new()),
        }
    }

    pub fn storage(&self) -> Storage {
        Storage::new(self.store.clone(), self.codec.clone(), Arc::new(TestExt))
    }

    /// A frontend named `term` over the store, started at farm time 0.
    pub fn core(&self, term: u64) -> Core<Rig> {
        self.core_with(&Config::new(term), Rig::default(), 0)
    }

    /// A frontend built from `config`, over `rig`, started at farm time `now`.
    pub fn core_with(&self, config: &Config, rig: Rig, now: u64) -> Core<Rig> {
        Core::open(config, rig, self.storage(), at(now)).expect("opened")
    }

    /// Ends `operation` in the store as another frontend does: its `done/` row, `value`.
    pub fn ended_elsewhere(&self, operation: OperationId, value: &[u8]) {
        let put = Txn::new().put(js_store::keys::done(operation), value.to_vec(), Cond::Any);
        self.store.write(put);
    }

    /// A handle that writes the rows as another frontend of the store does.
    pub fn rows(&self, frontend: u64) -> Rows {
        let (rows, _) = Rows::open(
            self.store.clone(),
            EpochPolicy::Keep,
            frontend,
            self.codec.clone(),
            Arc::new(TestExt),
        )
        .expect("opened");
        rows
    }
}

/// The operation `n`, as the rows name it.
pub fn operation(n: u64) -> OperationId {
    OperationId(n)
}
