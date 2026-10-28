//! The grant of a lease: the operation leaves the queue and holds the lease, in one
//! transaction.
//!
//! Several frontends may share one store and its rows, so the grant reads the rows it depends
//! on and writes conditionally on what it read. It deletes the `queue/` row at the version
//! read and puts the `lease/` row where there is none, so one operation never holds two
//! leases and a grant loses to another frontend's. It puts the operation's `lease-by-op/` row
//! beside the lease row (the index of leases by operation), and checks the `owner/` row of the lease's node
//! ([`Rows::guard`]): the transaction fails, whatever else it read, if the daemon has said
//! `Hello` to another frontend since this one did.
//!
//! A write the store applied and did not acknowledge is tried again, and finds its own work
//! in the rows: the lease row of the grant.

use js_state::{Cond, Txn};
use js_types::sched::LeaseGrant;

use crate::leases::lease_by_op_row;
use crate::{Rows, StateError, keys};

impl Rows {
    /// A grant committed: the operation leaves the queue and holds the lease of `grant`. The
    /// operation must be queued, as the transaction reads it. A grant that finds the
    /// operation not queued and the lease row naming this lease and worker is a write the
    /// store applied before without saying so: it is taken as written.
    ///
    /// # Errors
    /// [`StateError::Missing`] if the operation is not queued and holds no such lease,
    /// [`StateError::Conflict`] if its rows changed under the write,
    /// [`StateError::StaleOwner`] if the worker is another frontend's, or the store cannot
    /// be read or written, or a lease row read is not one the scheduler wrote.
    pub fn granted(&self, grant: &LeaseGrant) -> Result<(), StateError> {
        // First: a node another frontend owns is granted nothing, whatever the rows say.
        let txn = self.guard(Txn::new(), &grant.worker)?;
        let queued = keys::queue(grant.operation);
        let Some(row) = self.store.get(&queued)? else {
            return self.granted_before(grant, queued);
        };
        let value = self.codec.lease(grant)?;
        let lease = keys::lease(grant.lease);
        let index = keys::lease_by_op(grant.operation);
        let txn = txn
            .delete(queued, Cond::Version(row.version))
            .put(lease, value, Cond::Absent)
            .put(index, lease_by_op_row(grant.lease), Cond::Any);
        self.commit_owned(&grant.worker, txn)
    }

    /// The grant of an operation that is not queued: written already if the lease row is
    /// `grant`, by an earlier try of this write; stale otherwise.
    fn granted_before(&self, grant: &LeaseGrant, queued: String) -> Result<(), StateError> {
        let key = keys::lease(grant.lease);
        match self.store.get(&key)? {
            Some(row) if self.lease_row(&key, &row)? == *grant => Ok(()),
            _ => Err(StateError::Missing(queued)),
        }
    }
}
