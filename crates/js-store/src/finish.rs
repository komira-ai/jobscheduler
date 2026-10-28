//! The end of an operation: a result accepted, an operation refused, and the `done/` row that
//! is left.
//!
//! Several frontends may share one store and its rows, so each ending reads the rows it
//! depends on and writes conditionally on what it read, in one transaction. A frontend that
//! loses finds the rows changed or gone and writes nothing, and the other's work stands:
//!
//! - a result deletes the `lease/` row at the version read; if another frontend deletes it
//!   first, the result is made again as with no `lease/` row. With no `lease/` row, the
//!   result is the one of an operation whose lease the scheduler gave up in the same feed:
//!   its `queue/` and `op/` rows go at the versions read, and the result is put where there
//!   is none. If another writer changed those rows, the result is read: one already there
//!   means the operation was finished (by an earlier try of this write, or by another
//!   frontend), and this finish is a duplicate that is acknowledged, the first result
//!   standing;
//! - a refusal deletes the `queue/` and `op/` rows at the versions read, so it loses to a
//!   grant (which deletes the `queue/` row) of another frontend: an operation that is
//!   running is not refused. A refusal that finds its own result in the store is a
//!   duplicate too.
//!
//! The `done/` row of an [`Ending`] is put in the same transaction as the deletes, so the
//! store holds the operation's unfinished rows or its result, never neither. Its value is
//! the embedding server's bytes, and it expires after the finished retention, and at least
//! [`DONE_FLOOR`]. A result that finds its lease row also checks the `owner/` row of the
//! lease's node ([`Rows::guard`]): the transaction fails, whatever else it read, if the
//! daemon has said `Hello` to another frontend since this one did.
//!
//! A write the store applied and did not acknowledge is tried again, and finds its own work
//! in the rows: no lease row, and the result.

use std::time::Duration;

use js_state::{Cond, Txn};
use js_types::sched::{LeaseId, OperationId};

use crate::{Rows, StateError, keys};

/// The least time a `done/` row is kept, whatever the finished retention: a process that
/// follows an operation it did not finish needs the row for as long as a stream it kept
/// open can take to notice it.
pub const DONE_FLOOR: Duration = Duration::from_secs(600);

/// How an operation ended, as the `done/` row written in the transaction that removes the
/// operation's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// The value of the `done/` row.
    done: Vec<u8>,
    /// What the embedder keeps as long as the result ([`crate::Ext::ended`]), if anything.
    parcel: Option<Vec<u8>>,
    /// How long the row is kept, at least [`DONE_FLOOR`].
    retention: Duration,
}

impl Ending {
    /// An ending whose `done/` row holds `done`, kept for `retention` (and at least
    /// [`DONE_FLOOR`]).
    #[must_use]
    pub fn new(done: Vec<u8>, retention: Duration) -> Self {
        Self {
            done,
            parcel: None,
            retention,
        }
    }

    /// This ending, with `parcel` kept as long as the result ([`crate::Ext::ended`]).
    #[must_use]
    pub fn with_parcel(mut self, parcel: Vec<u8>) -> Self {
        self.parcel = Some(parcel);
        self
    }

    /// The value of the `done/` row.
    #[must_use]
    pub fn done(&self) -> &[u8] {
        &self.done
    }

    /// How long the result is kept: the finished retention, and at least [`DONE_FLOOR`], so
    /// that a client which reconnects to another frontend after the finish still finds it.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.retention.max(DONE_FLOOR)
    }
}

/// Where an operation stands in the state store, as a process that did not run it sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    /// It ended, and this is the value of its `done/` row.
    Ended(Vec<u8>),
    /// It waits for a lease.
    Queued,
    /// It holds a lease.
    Running,
    /// The store has no trace of it: not unfinished and not ended. Its writer ends an
    /// operation in one transaction (the operation's rows removed, its `done/` row put), so
    /// a finish never looks like this; a result row past its retention does.
    Gone,
}

impl Progress {
    /// Whether the store has no trace of the operation.
    #[must_use]
    pub const fn is_gone(&self) -> bool {
        matches!(self, Self::Gone)
    }
}

impl Rows {
    /// A result accepted from `lease`: the operation is finished. The lease row goes if it
    /// is there, compare-and-set on its version. It may be gone: the scheduler lets a
    /// worker's result decide an operation whose lease it gave up in the same feed, and
    /// that give-up already deleted the row and put the queue row back. Then the
    /// operation's `queue/` and `op/` rows go at the versions read, so a finished operation
    /// is never left queued, and the result goes only where there is none. A result already
    /// there is a duplicate of this finish, which is taken as written, the first standing.
    /// A lease row gone with the operation not queued is not that give-up:
    /// [`StateError::Missing`]. The lease row may also go between the read and the write,
    /// to a requeue or a finish of another frontend: the write is then made again as if the
    /// row had been gone at the read. A lease row that changed and is still there is
    /// another writer's: [`StateError::Conflict`].
    ///
    /// The `done/` row of `ending` is put in the same transaction as the deletes, so the
    /// store holds the operation's unfinished rows or its result, never neither.
    ///
    /// # Errors
    /// [`StateError::Missing`] if the operation is neither held under `lease` nor queued
    /// with a result of its own, [`StateError::Conflict`], [`StateError::StaleOwner`] if
    /// the lease's node is another frontend's, or the store cannot be read or written, or a
    /// row read is not one the scheduler wrote.
    pub fn finished(
        &self,
        operation: OperationId,
        lease: LeaseId,
        ending: &Ending,
    ) -> Result<(), StateError> {
        let key = keys::lease(lease);
        let Some(row) = self.store.get(&key)? else {
            return self.end_queued(operation, ending, &key, |_| true);
        };
        // The result of a lease is taken by the owner of the node that holds it.
        let worker = self.lease_row(&key, &row)?.worker;
        let txn = self.guard(Txn::new(), &worker)?;
        let txn = txn.delete(key.clone(), Cond::Version(row.version));
        let txn = self.unpin(txn, operation)?;
        let txn = self.noting(txn, operation, ending)?;
        let settled = self.settle_with(ending, txn, operation);
        let committed = self.commit_owned(&worker, settled);
        if matches!(committed, Err(StateError::Conflict(_))) && self.store.get(&key)?.is_none() {
            return self.end_queued(operation, ending, &key, |_| true);
        }
        committed
    }

    /// A refusal committed: the operation is finished without a lease, and its `done/` row
    /// goes in the same transaction as its rows are removed. The operation must be queued,
    /// as the transaction reads it: one that another frontend has granted is running, and
    /// is not refused. A result already there that is this refusal is a duplicate of it,
    /// which is taken as written; any other result stands, and the refusal is stale. The
    /// rows are read when the refusal is written, which may be after the store was
    /// unreachable for a while: an operation another frontend queued again in that time is
    /// refused all the same, on the judgment made when it was proposed.
    ///
    /// # Errors
    /// [`StateError::Missing`] if the operation is not queued (or another writer changed
    /// its rows since they were read) and has no result of this refusal; a lost race is
    /// read again, never returned as a conflict. Or the store cannot be read or written, or
    /// a row read is not one the scheduler wrote.
    pub fn refused(&self, operation: OperationId, ending: &Ending) -> Result<(), StateError> {
        let queued = keys::queue(operation);
        self.end_queued(operation, ending, &queued, |done| done == ending.done())
    }

    /// Ends `operation` in one transaction that deletes its `queue/` and `op/` rows at the
    /// versions read and puts the result of `ending` where there is none. If the rows are
    /// not both there, or another writer changed them since they were read, the result is
    /// read: `duplicate` says whether the one there is this ending's, and then it is taken
    /// as written. Otherwise the write is stale, naming `stale`.
    fn end_queued(
        &self,
        operation: OperationId,
        ending: &Ending,
        stale: &str,
        duplicate: impl Fn(&[u8]) -> bool,
    ) -> Result<(), StateError> {
        let queue = keys::queue(operation);
        let op = keys::operation(operation);
        let rows = (self.store.get(&queue)?, self.store.get(&op)?);
        if let (Some(queued), Some(unfinished)) = rows {
            let txn = Txn::new()
                .delete(queue, Cond::Version(queued.version))
                .delete(op, Cond::Version(unfinished.version));
            let txn = self.unpin(txn, operation)?;
            let txn = self.noting(txn, operation, ending)?;
            let settled = self.settle_rest(ending, txn, operation, Cond::Absent);
            let committed = self.commit(settled);
            if !matches!(committed, Err(StateError::Conflict(_))) {
                return committed;
            }
        }
        match self.ended(operation)? {
            Some(done) if duplicate(&done) => Ok(()),
            _ => Err(StateError::Missing(stale.to_owned())),
        }
    }

    /// `txn`, and what the embedder keeps as long as the result ([`crate::Ext::ended`]) if
    /// `ending` carries it and `operation` has an `op/` row: an operation with none is not
    /// known, and adds nothing.
    fn noting(&self, txn: Txn, operation: OperationId, ending: &Ending) -> Result<Txn, StateError> {
        let Some(parcel) = &ending.parcel else {
            return Ok(txn);
        };
        if self.store.get(&keys::operation(operation))?.is_none() {
            return Ok(txn);
        }
        Ok(self.ext.ended(txn, operation, parcel, ending.ttl()))
    }

    /// `txn`, then the removal of the operation's `op/`, `queue/`, `loss/`, `fate/` and
    /// `opkey/` rows (and the rows the embedder's [`crate::Ext::settled`] removes, between
    /// the `fate/` and `opkey/` deletes) and the put of its result, so one transaction
    /// carries all of it. The `opkey/` delete is made whether or not size keys are read
    /// ([`Rows::set_read_size_keys`]): an operation queued while they were on has the row,
    /// and a later run with them off cleans it up as the operation ends. The result goes
    /// over whatever is there: the transaction also deletes the lease row at the version
    /// read, which no other writer can have deleted first and written a result after.
    fn settle_with(&self, ending: &Ending, txn: Txn, operation: OperationId) -> Txn {
        let txn = txn
            .delete(keys::operation(operation), Cond::Any)
            .delete(keys::queue(operation), Cond::Any);
        self.settle_rest(ending, txn, operation, Cond::Any)
    }

    /// `txn`, which already deletes the operation's `op/` and `queue/` rows at the versions
    /// it read, then the removal of the rest of its rows (its `lease-by-op/` row among them,
    /// whoever left it) and the put of its result where `done` allows: [`Cond::Absent`] for a
    /// result no other writer has put.
    fn settle_rest(&self, ending: &Ending, txn: Txn, operation: OperationId, done: Cond) -> Txn {
        let txn = txn
            .delete(keys::loss(operation), Cond::Any)
            .delete(keys::fate(operation), Cond::Any);
        self.ext
            .settled(txn, operation)
            .delete(keys::opkey(operation), Cond::Any)
            .delete(keys::lease_by_op(operation), Cond::Any)
            .put_ttl(
                keys::done(operation),
                ending.done.clone(),
                ending.ttl(),
                done,
            )
    }

    /// The value of `operation`'s `done/` row, if the row is still there: how it ended. The
    /// row is the answer a process that did not finish the operation gives the callers it
    /// kept waiting.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn ended(&self, operation: OperationId) -> Result<Option<Vec<u8>>, StateError> {
        let row = self.store.get(&keys::done(operation))?;
        Ok(row.map(|row| row.value))
    }

    /// Where `operation` stands. The reads are separate, so a finish may land between them:
    /// `done/` is read again when the operation's rows are gone, and a finish (one
    /// transaction) leaves its result by then.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn progress(&self, operation: OperationId) -> Result<Progress, StateError> {
        if let Some(done) = self.ended(operation)? {
            return Ok(Progress::Ended(done));
        }
        let unfinished = self.store.get(&keys::operation(operation))?.is_some();
        let queued = self.store.get(&keys::queue(operation))?.is_some();
        Ok(match (unfinished, queued) {
            (false, _) => self
                .ended(operation)?
                .map_or(Progress::Gone, Progress::Ended),
            (true, true) => Progress::Queued,
            (true, false) => Progress::Running,
        })
    }
}
