//! Queueing an operation: its `op/` and `queue/` rows, the `inflight/` row that lets a twin
//! request join it, the `opkey/` row of its size key and the embedder's rows ([`crate::Ext`]),
//! in one transaction.
//!
//! Submitting is safe to repeat: a commit whose outcome the caller never learned (the
//! connection died after it) leaves the operation written, and the retry finds it through its
//! `inflight/` row and joins it instead of queueing the work a second time.

use js_sched::Request;
use js_state::{Cond, Txn};
use js_types::sched::OperationId;

use crate::load::StoredOperation;
use crate::{Rows, StateError, keys, number, opkey};

/// How many times a submit looks again after a write finds a row in its way.
const LOOKS: usize = 3;

/// What a submit did.
#[derive(Debug)]
pub enum Submitted {
    /// The operation is written, under this id.
    Queued(OperationId),
    /// The store holds an operation that runs the work: the request joins it.
    Joined(Box<StoredOperation>),
}

impl Rows {
    /// Queues `request` as operation `id`, with `parcel` for the embedder's rows and the
    /// size key `size_key`, if it has one, in an `opkey/` row of the same transaction; unless
    /// the store holds an operation that runs the work: then that one is returned, to join.
    /// Safe to repeat: a write whose outcome the caller never learned has put the row, and
    /// the repeat finds the operation it wrote.
    ///
    /// `id` comes from the block this handle reserved ([`Rows::reserve_operation`]), where
    /// no other frontend numbers. If a row of an earlier writer is in the way of it, the
    /// next id of the block is used, at most three times; [`Submitted::Queued`] says which.
    ///
    /// # Errors
    /// The store cannot be read or written, a row read is not one the scheduler wrote (the
    /// in-flight row names an operation of other work: [`StateError::Corrupt`]), or a row in
    /// the way does not go ([`StateError::Conflict`]).
    pub fn submit(
        &self,
        mut id: OperationId,
        request: &Request,
        parcel: &[u8],
        size_key: Option<u64>,
    ) -> Result<Submitted, StateError> {
        if !request.joinable() {
            self.commit(self.submission(id, request, parcel, size_key)?)?;
            return Ok(Submitted::Queued(id));
        }
        let pin = keys::inflight(&request.key);
        let mut looks = 0;
        loop {
            let mut cond = Cond::Absent;
            if let Some(row) = self.store.get(&pin)? {
                let held = OperationId(number(&pin, &row)?);
                if let Some(op) = self.stored_operation(held)? {
                    if op.request.key != request.key {
                        return Err(StateError::corrupt(
                            &pin,
                            "names an operation of other work",
                        ));
                    }
                    return Ok(Submitted::Joined(Box::new(op)));
                }
                // The operation it names is gone: the row is left over, and goes.
                cond = Cond::Version(row.version);
            }
            let txn = self.submission(id, request, parcel, size_key)?;
            match self.commit(txn.put(pin.clone(), id.0.to_be_bytes(), cond)) {
                Ok(()) => return Ok(Submitted::Queued(id)),
                // Another writer queued this work since the look, or a row of an earlier
                // writer is in the way of the id: look again, and try the next id of the
                // block.
                Err(StateError::Conflict(_)) if looks < LOOKS => {
                    looks += 1;
                    id = self.reserve_operation(OperationId(id.0.saturating_add(1)))?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The transaction that queues `request` as operation `operation`: every row is put
    /// only if absent, so a second submission of one id is refused over the first.
    fn submission(
        &self,
        operation: OperationId,
        request: &Request,
        parcel: &[u8],
        size_key: Option<u64>,
    ) -> Result<Txn, StateError> {
        let value = self.codec.submit(request)?;
        let mut txn = Txn::new();
        if let Some(key) = size_key {
            txn = txn.put(keys::opkey(operation), opkey::encode(key), Cond::Absent);
        }
        let txn = txn.put(keys::operation(operation), value, Cond::Absent);
        let txn = self.ext.submitted(txn, operation, parcel);
        Ok(txn.put(keys::queue(operation), Vec::new(), Cond::Absent))
    }
}
