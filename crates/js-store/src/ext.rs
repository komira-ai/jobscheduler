//! The rows only an embedding server has ([`Ext`]).
//!
//! This crate writes the rows every scheduler needs, and calls its `Ext` to add the
//! embedder's own to the transactions it builds (a submission, the end of an operation) and
//! to read them back. What an embedder keeps with an operation is its own: this crate sees
//! it only as bytes (a parcel) and the operation's id. The farm server keeps what the
//! `action usage:` line says an operation is when it is submitted, and the name and action
//! of an operation for as long as its result.
//!
//! An `Ext` adds rows to the transaction it is given and never reads, writes or deletes a
//! row this crate owns, so the transaction stays one and the rows of an operation are in
//! the store together or not at all. Its rows have prefixes of their own, which a release
//! that does not know them never scans.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::{Store, Txn};
use js_types::sched::OperationId;

use crate::StateError;

/// The rows of an embedding server, beside the scheduler's.
pub trait Ext: Send + Sync {
    /// `txn`, and the rows of the operation a submission queues, holding `parcel`.
    fn submitted(&self, txn: Txn, operation: OperationId, parcel: &[u8]) -> Txn;

    /// `txn`, and the removal of the rows `operation` leaves when it ends.
    fn settled(&self, txn: Txn, operation: OperationId) -> Txn;

    /// `txn`, and the put of the row that keeps `parcel` as long as the result of
    /// `operation`, which ends in `txn`: for `ttl`.
    fn ended(&self, txn: Txn, operation: OperationId, parcel: &[u8], ttl: Duration) -> Txn;

    /// The parcel `operation` was submitted with, if its rows are there.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a parcel.
    fn parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError>;

    /// The parcel of every operation that has rows.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not a parcel.
    fn parcels(&self, store: &dyn Store) -> Result<BTreeMap<OperationId, Vec<u8>>, StateError>;

    /// The parcel the finished or refused `operation` was kept with, if the store kept it.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a parcel.
    fn ended_parcel(
        &self,
        store: &dyn Store,
        operation: OperationId,
    ) -> Result<Option<Vec<u8>>, StateError>;
}
