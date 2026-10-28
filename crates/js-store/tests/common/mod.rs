//! Helpers the test files share: a store that lets another frontend's write land in the
//! middle of one of this frontend's, and a handle over a store.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
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
    asked: Mutex<Vec<Txn>>,
    lease_scans: AtomicUsize,
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
            asked: Mutex::new(Vec::new()),
            lease_scans: AtomicUsize::new(0),
        })
    }

    /// How many times every `lease/` row has been scanned.
    pub fn lease_scans(&self) -> usize {
        self.lease_scans.load(Ordering::SeqCst)
    }

    /// Every transaction the store was asked to apply since the last call, in order, as it
    /// was asked: whether it committed or not. The transactions of a landing are not among
    /// them.
    pub fn asked(&self) -> Vec<Txn> {
        std::mem::take(&mut *self.asked.lock().expect("lock"))
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
        self.asked
            .lock()
            .expect("lock")
            .extend(batch.iter().cloned());
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
        self.lease_scans
            .fetch_add(usize::from(prefix == "lease/"), Ordering::SeqCst);
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

/// The bytes of the hex digits `text`.
pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digits"))
        .collect()
}

/// The rows of `store` under `prefixes`, a line each: key, expiry and value as hex.
pub fn dump(store: &Fake, prefixes: &[&str]) -> String {
    let mut out = String::new();
    for (key, row) in store.scan_prefix("").expect("scan") {
        if !prefixes.iter().any(|prefix| key.starts_with(prefix)) {
            continue;
        }
        let expiry = row.expires_at.map_or_else(
            || "-".to_owned(),
            |at| at.as_duration().as_nanos().to_string(),
        );
        let value: String = row.value.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let _ = writeln!(out, "{key} expires_ns={expiry} value={value}");
    }
    out
}

/// The transaction `txn` as the farm server's trace of the store writes it, a line for each
/// operation: the kind, the key, the condition (`any`, `absent` or `version`, whichever
/// version) and, for a put, the lifetime.
pub fn shape(txn: &Txn) -> Vec<String> {
    let cond = |cond: Cond| match cond {
        Cond::Any => "any",
        Cond::Absent => "absent",
        Cond::Version(_) => "version",
    };
    txn.ops()
        .iter()
        .map(|op| match op {
            Op::Put {
                key,
                ttl,
                cond: held,
                ..
            } => {
                let ttl = ttl.map_or_else(|| "-".to_owned(), |t| format!("{}s", t.as_secs()));
                format!("put {key} cond={} ttl={ttl}", cond(*held))
            }
            Op::Delete { key, cond: held } => format!("del {key} cond={}", cond(*held)),
            Op::Check { key, cond: held } => format!("check {key} cond={}", cond(*held)),
            Op::Next { counter } => format!("next {counter}"),
        })
        .collect()
}

/// The only transaction `store` was asked for since the last look, as [`shape`] shows it.
pub fn only(store: &Racing) -> Vec<String> {
    let asked = store.asked();
    let [txn] = &asked[..] else {
        panic!("one transaction expected, found {}", asked.len());
    };
    shape(txn)
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
