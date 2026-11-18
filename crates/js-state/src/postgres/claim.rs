//! The claim paths of the Postgres store: [`PostgresStore::claim`], which takes rows under a
//! prefix that no other transaction holds, and the decided claim, which locks the rows
//! it names and decides one transaction on what it reads in the same transaction
//! ([`crate::Store::claim_decide`]).
//!
//! Both run their callers' closures inside the transaction. A claim takes the commit head only
//! at its tail, like a batch. A decided claim takes it first, before it reads anything, and
//! holds it to its commit: the store's contract is that what a decision reads, a scan of a
//! prefix included, is what its commit finds, and no lock on rows keeps a scanned range from
//! gaining a row. Its reads take no lock (no writer can commit until it ends), so a decision
//! never waits on a row another transaction's body holds while that body waits for the head;
//! only the keys its transaction writes are locked, when it applies. Such a key can be held by
//! a batch whose body has run and which waits for the head at its tail: the server finds that
//! deadlock (after its `deadlock_timeout`) and rolls one of the two back, and the store runs it
//! again. The keys a placement writes are its own job's, so the two meet only when another
//! write of that job runs at the same moment. The closures must not
//! call the store: they run inside a transaction that holds a connection, and the decision
//! holds the head. A transaction the server rolls back for a deadlock is run again from the
//! start, closures included.

use super::tx::{Revs, Tx};
use super::{Claimed, Fail, Flaw, PostgresStore, describe, versioned};
use crate::store::{Claim, TxnRead};
use crate::{StoreError, Txn, Versioned};

impl PostgresStore {
    /// Claims up to `limit` live rows under `prefix`, in key order, skipping rows another
    /// transaction holds. `make` turns each row into the transaction that claims it
    /// (typically: delete the queue entry on its version, write a lease); `None` passes
    /// the row over, and a transaction whose precondition fails is a lost race and is
    /// passed over too. All claims commit together.
    ///
    /// The transaction runs read committed: every statement reads the latest committed
    /// rows, and the claimed rows are locked. One the server rolls back for a deadlock is
    /// run again, `make` included.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails; nothing is claimed then.
    pub fn claim(
        &self,
        prefix: &str,
        limit: usize,
        mut make: impl FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, StoreError> {
        self.retrying(|| self.claim_in(prefix, limit, &mut make))
    }

    fn claim_in(
        &self,
        prefix: &str,
        limit: usize,
        make: &mut dyn FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, Fail> {
        let mut tx = self.start()?;
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let skip_locked = self.flaw != Flaw::ClaimBlocks;
        let rows = tx.claim_rows(prefix, take, skip_locked)?;
        for (key, row) in &rows {
            tx.remember(key.clone(), row.clone());
        }
        let mut claimed = Vec::new();
        for (key, row) in rows {
            let was = versioned(&row);
            let Some(txn) = make(&key, &was) else {
                continue;
            };
            if let Ok(commit) = tx.apply(&txn)? {
                claimed.push(Claimed { key, was, commit });
            }
        }
        let revs: Revs = tx.flush()?;
        let claimed: Vec<Claimed> = claimed
            .into_iter()
            .map(|claim| Claimed {
                commit: revs.settle(claim.commit),
                ..claim
            })
            .collect();
        self.commit(tx, claimed.len())?;
        Ok(claimed)
    }

    /// [`crate::Store::claim_decide`], run again when the server rolls it back for a deadlock.
    pub(super) fn claim_decide_retrying(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, StoreError> {
        self.retrying(|| self.claim_decide_in(claim, decide))
    }

    fn claim_decide_in(
        &self,
        claim: &[String],
        decide: &mut dyn FnMut(&mut dyn TxnRead) -> Option<Txn>,
    ) -> Result<Claim, Fail> {
        let mut tx = self.start()?;
        tx.hold_head()?;
        let locked = tx.lock_keys(claim)?;
        if locked.len() != claim.len() {
            return Ok(Claim::Held);
        }
        let mut reads = TxReads {
            tx: &mut tx,
            failed: None,
        };
        let decided = decide(&mut reads);
        if let Some(err) = reads.failed {
            return Err(Fail::Pg(err));
        }
        let Some(txn) = decided else {
            return Ok(Claim::Declined);
        };
        match tx.apply(&txn)? {
            Err(conflict) => Ok(Claim::Conflict(conflict)),
            Ok(commit) => {
                let commit = tx.flush()?.settle(commit);
                self.commit(tx, 1)?;
                Ok(Claim::Committed(commit))
            }
        }
    }
}

/// The reads a decision of [`PostgresStore::claim_decide`] makes inside its transaction. A
/// failed read is kept, not returned through the decision (which returns a transaction), and
/// fails the call after it.
struct TxReads<'a, 'p> {
    tx: &'a mut Tx<'p>,
    failed: Option<postgres::Error>,
}

impl TxReads<'_, '_> {
    fn answer<T>(&mut self, read: Result<T, postgres::Error>) -> Result<T, StoreError> {
        read.map_err(|err| {
            let said = StoreError::Failed(describe(&err));
            self.failed = Some(err);
            said
        })
    }
}

impl TxnRead for TxReads<'_, '_> {
    fn get(&mut self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let row = self.tx.peek(key);
        self.answer(row).map(|row| row.as_ref().map(versioned))
    }

    fn scan_prefix(&mut self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let rows = self.tx.read_prefix(prefix);
        self.answer(rows).map(|rows| {
            rows.into_iter()
                .map(|(key, row)| (key, versioned(&row)))
                .collect()
        })
    }
}
