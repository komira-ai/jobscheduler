//! [`Checked`]: a store whose every writing transaction also checks one key, so a writer that
//! holds a claim on that key (the connection of a host, say) commits only while the claim holds.

use std::sync::Arc;

use crate::clock::StoreTime;
use crate::store::{Claim, Store, TxnRead};
use crate::types::{
    Changes, Commit, CommitStats, Cond, Conflict, Op, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// A store that adds `Check { key, cond }` to every transaction that writes (a put, a delete
/// or a counter), so the transaction conflicts, writing nothing, once `key` no longer meets
/// `cond`. A transaction that only reads the head (an empty one) is passed as it is, and every
/// read goes to the store as it is.
pub struct Checked<S: ?Sized> {
    inner: Arc<S>,
    key: String,
    cond: Cond,
}

impl<S: ?Sized> Checked<S> {
    /// `inner`, with every writing transaction checking that `key` meets `cond`.
    #[must_use]
    pub fn new(inner: Arc<S>, key: impl Into<String>, cond: Cond) -> Checked<S> {
        Checked {
            inner,
            key: key.into(),
            cond,
        }
    }

    /// `txn` with the check, when it writes.
    fn with_check(&self, txn: Txn) -> Txn {
        let writes = txn
            .ops()
            .iter()
            .any(|op| matches!(op, Op::Put { .. } | Op::Delete { .. } | Op::Next { .. }));
        match writes {
            true => txn.check(self.key.as_str(), self.cond),
            false => txn,
        }
    }
}

impl<S: Store + ?Sized> Store for Checked<S> {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let checked = batch.into_iter().map(|txn| self.with_check(txn)).collect();
        self.inner.apply_batch(checked)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
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

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
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

    fn head(&self) -> Result<Option<Revision>, StoreError> {
        self.inner.head()
    }

    fn claim_decide(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        self.inner.claim_decide(claim, &mut |read| {
            decide(read).map(|txn| self.with_check(txn))
        })
    }
}
