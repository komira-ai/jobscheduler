//! The `inflight/<instance>/<action>` row: which operation runs a piece of work now, so a
//! request for work already queued attaches to that operation, whichever process queued it
//! and whether or not this one saw the write land.
//!
//! The key is the request's dedup key ([`js_types::sched::WorkKey`]): the class in
//! hexadecimal (it may hold any character, a `/` too), `/`, and the dedup text
//! ([`crate::keys::inflight`]); the value is the operation's id, 8 bytes big-endian. It is
//! written in the transaction that queues the operation, put only if absent, and removed in
//! the one that finishes it ([`Rows::unpin`]). A request that is not joinable
//! ([`js_sched::Request::joinable`]) has no row: its runs are not interchangeable.
//!
//! The key is the work, not the call, because that is what the scheduler joins on already;
//! a caller's retry after a cut stream or a lost acknowledgement names the same work and so
//! finds the same row, with no request id to carry through the front.

use js_state::{Cond, Txn};
use js_types::sched::OperationId;

use crate::{Rows, StateError, keys, number};

impl Rows {
    /// `txn`, and the removal of `operation`'s in-flight row if it names `operation`: the
    /// finish that removes the operation's rows takes its pin with it. A row that names
    /// another operation (this one's row was removed and a later operation of the same
    /// work took the key) stays, and so does the row of an operation that has no `op/`
    /// row (a finish that landed already).
    ///
    /// # Errors
    /// The store cannot be read, or a row read is not one the scheduler wrote.
    pub fn unpin(&self, txn: Txn, operation: OperationId) -> Result<Txn, StateError> {
        let key = keys::operation(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(txn);
        };
        let request = self.request_row(&key, &row)?;
        let pin = keys::inflight(&request.key);
        Ok(match self.store.get(&pin)? {
            Some(row) if number(&pin, &row)? == operation.0 => {
                txn.delete(pin, Cond::Version(row.version))
            }
            _ => txn,
        })
    }
}
