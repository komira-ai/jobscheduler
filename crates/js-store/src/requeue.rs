//! A lease given up: the operation holds nothing and is queued again, in one transaction that
//! also writes the loss count, the fate and the memory floors the give-up changes.
//!
//! Several frontends may share one store and its rows, so the give-up reads the rows it
//! depends on and writes conditionally on what it read. It deletes the `lease/` row at the
//! version read, checks that the `op/` row is still the one read, and puts the `queue/` row
//! where there is none. With no `lease/` row it writes nothing of the operation: another
//! frontend finished or requeued it (or an earlier try of this very write did), and putting
//! the `queue/` row would run a finished operation again or queue a running one beside its
//! lease. Like the grant, it checks the `owner/` row of the lease's node ([`Rows::guard`]):
//! the transaction fails, whatever else it read, if the daemon has said `Hello` to another
//! frontend since this one did. It deletes the operation's `lease-by-op/` row, as every ending
//! does.
//!
//! The memory floors a give-up raises are written with it, each at the version read of its
//! row and with the larger ask of the row and its own: a frontend whose kills raised a floor
//! less far than another's never lowers it, and one that finds the row changed between its
//! read and its write loses ([`StateError::Conflict`]), and writes the give-up again from the
//! row as it stands. A floor outlives its operation and is the farm's, not the node's owner's:
//! the floors of a give-up that is refused as stale for the owner are written all the same,
//! and the refusal is not tried again.
//!
//! A write the store applied and did not acknowledge is tried again, and finds its own work in
//! the rows: no lease row to give up.

use js_sched::{Fate, FloorChange, KeyFloor, Requeue};
use js_state::{Cond, Txn};

use crate::floor::{self, found};
use crate::{Rows, StateError, fate, keys};

/// Whether `floors` changes any `floor/` or `keyfloor/` row.
fn changes_rows(floors: &FloorChange) -> bool {
    floors.raised.is_some()
        || !floors.evicted.is_empty()
        || floors.key_raised.is_some()
        || !floors.key_evicted.is_empty()
}

impl Rows {
    /// A lease given up: the operation holds nothing and is queued again, if it still held
    /// the lease. Repeatable, and safe beside other frontends: a give-up the store refused is
    /// written again until the store takes it, and a repeat after it was taken finds the lease
    /// row gone and writes nothing, as a give-up of a lease that another frontend finished or
    /// requeued does. An operation that has finished has nothing to put back.
    ///
    /// `losses` is the operation's count of lost leases, to write down if the give-up is one
    /// ([`js_sched::RequeueReason::is_loss`]). `fate` is what the operation's faults changed so
    /// far, if they changed anything: it goes in the same transaction as the queue row, each
    /// over the version it read of the row. `summary` is the name of the size summary the
    /// raised key floor decays with, if it is known: its row keeps it.
    ///
    /// The loss count and the fate are written only if the store held the lease: a grant the
    /// scheduler gave up before the store took it has no lease row, the operation is still
    /// queued there, and nothing of that lease is written, so a restart does not count it.
    ///
    /// The memory floors the give-up changes are written whether or not the operation is
    /// queued again: a floor is the scheduler's, and outlives the operation.
    ///
    /// # Errors
    /// [`StateError::StaleOwner`] if the lease's node is another frontend's (the floors are
    /// written first); [`StateError::Conflict`] if a row changed under the write; the store
    /// cannot be read or written; a fate or a floor is too large to write
    /// ([`StateError::Corrupt`]).
    pub fn requeued(
        &self,
        requeue: &Requeue,
        losses: Option<u32>,
        fate: Option<&Fate>,
        floors: &FloorChange,
        summary: Option<&str>,
    ) -> Result<(), StateError> {
        let mut txn = Txn::new();
        // The floor outlives the operation: a kill raises it, and the operation may end before
        // the write is taken.
        let raised = floors.raised.as_ref();
        if let Some(floor) = raised {
            // Another frontend may have raised the floor further: the row keeps the larger
            // ask, and is written at the version read.
            let (merged, cond) = self.merged_floor(floor)?;
            let key = keys::floor(&floor.key);
            let row = floor::encode(&merged)
                .ok_or_else(|| StateError::corrupt(&key, "a floor is too large to write"))?;
            txn = txn.put(key, row, cond);
        }
        // A key forgotten and raised again since keeps its row.
        for key in floors
            .evicted
            .iter()
            .filter(|k| raised.is_none_or(|f| &f.key != *k))
        {
            txn = txn.delete(keys::floor(key), Cond::Any);
        }
        // The same for the key floor, in the same transaction.
        let key_raised = floors.key_raised.as_ref();
        if let Some(floor) = key_raised {
            let (merged, cond) = self.merged_key_floor(floor)?;
            let value = self.key_floor_value(&merged, summary)?;
            txn = txn.put(keys::key_floor(floor.key), value, cond);
        }
        for key in floors
            .key_evicted
            .iter()
            .filter(|k| key_raised.is_none_or(|f| f.key != **k))
        {
            txn = txn.delete(keys::key_floor(*key), Cond::Any);
        }
        let floors_changed = changes_rows(floors);
        let (op, lease) = (
            keys::operation(requeue.operation),
            keys::lease(requeue.lease),
        );
        let rows = (self.store.get(&op)?, self.store.get(&lease)?);
        let (Some(unfinished), Some(held)) = rows else {
            // The operation ended, or its lease is not held in the store any more: nothing is
            // put back in the queue.
            return if floors_changed {
                self.commit(txn)
            } else {
                Ok(())
            };
        };
        // The lease is the node's owner's to give up. The floors are the scheduler's, and are
        // written whoever owns the node: a give-up refused for the owner is not tried again.
        txn = match self.guard(txn.clone(), &requeue.worker) {
            Ok(guarded) => guarded,
            Err(refused) => {
                if floors_changed {
                    self.commit(txn)?;
                }
                return Err(refused);
            }
        };
        txn = txn
            .check(op, Cond::Version(unfinished.version))
            .delete(lease, Cond::Version(held.version))
            .delete(keys::lease_by_op(requeue.operation), Cond::Any);
        if let Some(count) = losses {
            let key = keys::loss(requeue.operation);
            let cond = self.current(&key)?;
            txn = txn.put(key, u64::from(count).to_be_bytes(), cond);
        }
        if let Some(fate) = fate {
            let key = keys::fate(requeue.operation);
            let row = fate::encode(fate)
                .ok_or_else(|| StateError::corrupt(&key, "a fate is too large to write"))?;
            let cond = self.current(&key)?;
            txn = txn.put(key, row, cond);
        }
        let txn = txn.put(keys::queue(requeue.operation), Vec::new(), Cond::Absent);
        self.commit_owned(&requeue.worker, txn)
    }

    /// Writes the key floor `floor` as it now stands, or deletes its row when it was forgotten
    /// (an `ask` of 0), whatever the row holds: outside any give-up. A decay that lowers a
    /// floor writes it with [`Rows::lower_key_floor`], which does not overwrite a floor
    /// another frontend raised. The row keeps its stamp, and names the size summary
    /// `summary` that its floor decays with, or the one it already names.
    ///
    /// # Errors
    /// The store cannot be read or written.
    pub fn put_key_floor(&self, floor: &KeyFloor, summary: Option<&str>) -> Result<(), StateError> {
        let row = keys::key_floor(floor.key);
        let txn = if floor.ask == 0 {
            Txn::new().delete(row, Cond::Any)
        } else {
            Txn::new().put(row, self.key_floor_value(floor, summary)?, Cond::Any)
        };
        self.commit(txn)
    }

    /// What a write that replaces the row at `key` must find: the version it has now, or no
    /// row.
    fn current(&self, key: &str) -> Result<Cond, StateError> {
        Ok(found(self.store.get(key)?))
    }
}
