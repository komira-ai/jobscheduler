//! A [`Store`] that forwards to another and remembers every write it was asked for, every
//! read that names keys or the clock and every notice it was asked to send, so a test can
//! assert which keys a component touched (and with what kind of operation), what it read and
//! what it told, after the fact. Test support: it lives here and not in `js-testkit` because
//! that crate depends on no kbf crate, and a store wrapper needs this one.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::clock::StoreTime;
use crate::store::{Claim, Store, TxnRead};
use crate::types::{
    Changes, Commit, CommitStats, Conflict, Notice, Op, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// What a recorded write did to its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    /// A put, with or without a time to live.
    Put,
    /// A delete.
    Delete,
    /// A counter increment; the recorded key is the counter's name.
    Next,
}

/// One write a [`RecordingStore`] was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// The key (or the counter's name for [`WriteKind::Next`]).
    pub key: String,
    /// The kind of operation.
    pub kind: WriteKind,
    /// Whether its transaction committed. A write refused (a failed precondition, a
    /// store failure) is recorded too, with `false`: the question a test asks is usually
    /// which keys a component *tried* to write.
    pub committed: bool,
}

/// One read a [`RecordingStore`] was asked for. The feed, the head and the counters are not
/// recorded, nor what a decision of [`Store::claim_decide`] reads inside its transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// A get of the key.
    Get(String),
    /// A get of the keys at once ([`Store::get_many`]), as they were asked for.
    GetMany(Vec<String>),
    /// A prefix scan under the prefix.
    ScanPrefix(String),
    /// A range scan (C8) under the prefix.
    ScanRange(String),
    /// A read of the store's clock ([`Store::now`]).
    Now,
}

#[derive(Default)]
struct Log {
    writes: Vec<Write>,
    reads: Vec<Read>,
    reaps: usize,
    notices: Vec<Notice>,
}

/// Forwards every call to the store underneath and records the writes and the reads ([`Read`]).
/// A precondition-only operation (a check) is not a write and is not recorded.
pub struct RecordingStore {
    inner: Arc<dyn Store>,
    log: Mutex<Log>,
}

impl RecordingStore {
    /// Records the writes made through the returned store to `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Store>) -> RecordingStore {
        RecordingStore {
            inner,
            log: Mutex::new(Log::default()),
        }
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every recorded write, in the order it was asked for (the operations of one
    /// transaction in order, a batch's transactions in order).
    #[must_use]
    pub fn writes(&self) -> Vec<Write> {
        self.log().writes.clone()
    }

    /// Every recorded read, in the order it was asked for.
    #[must_use]
    pub fn reads(&self) -> Vec<Read> {
        self.log().reads.clone()
    }

    /// The distinct keys written, sorted.
    #[must_use]
    pub fn keys(&self) -> BTreeSet<String> {
        self.log().writes.iter().map(|w| w.key.clone()).collect()
    }

    /// How many reaps were asked for (a reap writes `Expire` rows for keys the caller
    /// does not name, so it is counted, not listed).
    #[must_use]
    pub fn reaps(&self) -> usize {
        self.log().reaps
    }

    /// Every notice sent through this store ([`Store::notify`]), in the order sent.
    #[must_use]
    pub fn notices(&self) -> Vec<Notice> {
        self.log().notices.clone()
    }

    /// Forgets what was recorded so far: the writes, the reads, the reaps and the notices.
    pub fn clear(&self) {
        *self.log() = Log::default();
    }

    fn read(&self, read: Read) {
        self.log().reads.push(read);
    }

    fn record(&self, txn: &Txn, committed: bool) {
        let mut log = self.log();
        for op in txn.ops() {
            let (key, kind) = match op {
                Op::Put { key, .. } => (key, WriteKind::Put),
                Op::Delete { key, .. } => (key, WriteKind::Delete),
                Op::Next { counter } => (counter, WriteKind::Next),
                Op::Check { .. } => continue,
            };
            log.writes.push(Write {
                key: key.clone(),
                kind,
                committed,
            });
        }
    }
}

impl Store for RecordingStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let result = self.inner.apply_batch(batch.clone());
        for (index, txn) in batch.iter().enumerate() {
            let committed =
                matches!(&result, Ok(results) if matches!(results.get(index), Some(Ok(_))));
            self.record(txn, committed);
        }
        result
    }

    fn head(&self) -> Result<Option<Revision>, StoreError> {
        self.inner.head()
    }

    fn claim_decide(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        let mut decided = None;
        let result = self.inner.claim_decide(claim, &mut |read| {
            decided = decide(read);
            decided.clone()
        });
        if let Some(txn) = decided {
            self.record(&txn, matches!(result, Ok(Claim::Committed(_))));
        }
        result
    }

    fn txn_stamped(
        &self,
        build: &mut dyn FnMut(StoreTime) -> Option<Txn>,
    ) -> Result<Option<Result<Commit, Conflict>>, StoreError> {
        let mut built = None;
        let result = self.inner.txn_stamped(&mut |now| {
            built = build(now);
            built.clone()
        });
        if let Some(txn) = built {
            self.record(&txn, matches!(result, Ok(Some(Ok(_)))));
        }
        result
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.read(Read::Get(key.to_owned()));
        self.inner.get(key)
    }

    fn get_many(&self, keys: &[String]) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.read(Read::GetMany(keys.to_vec()));
        self.inner.get_many(keys)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.read(Read::ScanPrefix(prefix.to_owned()));
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.read(Read::ScanRange(scan.prefix.to_owned()));
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.log().reaps += 1;
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> Result<StoreTime, StoreError> {
        self.read(Read::Now);
        self.inner.now()
    }

    fn notify(&self, notices: &[Notice]) {
        self.log().notices.extend_from_slice(notices);
        self.inner.notify(notices);
    }
}
