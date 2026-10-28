//! What several frontends of one state store share: the blocks of operation ids and lease
//! sequences each reserves with a compare-and-set on the counter row.

use std::ops::Range;
use std::sync::PoisonError;

use js_state::{Cond, Txn};
use js_types::sched::OperationId;

use crate::keys::{NEXT_OP, NEXT_SEQ};
use crate::{Rows, StateError, number};

/// How many operation ids, or lease sequences, one reservation takes from `meta/next_op`
/// or `meta/next_seq`. A frontend writes the counter once for this many ids, so a block
/// is as many operations as a frontend submits, or leases it grants, between two writes
/// of the counter. A frontend that stops leaves the rest of its block unused: ids are 64
/// bits, and an unused id costs nothing.
pub const ID_BLOCK: u64 = 1024;

/// How many times in all a compare-and-set that another frontend wrote under is tried
/// again, reading the row again each time.
pub const CAS_TRIES: usize = 8;

/// Runs `attempt`, and again while it loses a compare-and-set to another writer (it reads
/// the rows it depends on again each time), at most [`CAS_TRIES`] times in all. The last
/// conflict is returned.
pub(crate) fn contended<T>(
    mut attempt: impl FnMut() -> Result<T, StateError>,
) -> Result<T, StateError> {
    let mut tried = 1;
    loop {
        match attempt() {
            Err(StateError::Conflict(_)) if tried < CAS_TRIES => tried += 1,
            done => return done,
        }
    }
}

impl Rows {
    /// Reserves a block of [`ID_BLOCK`] ids of `counter` (`meta/next_op` or
    /// `meta/next_seq`) for this frontend: the stored value, or `own` if that is later, up to
    /// [`ID_BLOCK`] past it. The counter is written to the end of the block with a
    /// compare-and-set on the version read, so it never goes back and no other frontend's
    /// block overlaps; a write that loses to another frontend reads the counter again.
    ///
    /// # Errors
    /// The store cannot be read or written, the counter is not a number, or other
    /// frontends won every one of [`CAS_TRIES`] tries ([`StateError::Conflict`]).
    fn reserve(&self, counter: &str, own: u64) -> Result<Range<u64>, StateError> {
        contended(|| {
            let (stored, cond) = match self.store.get(counter)? {
                Some(row) => (number(counter, &row)?, Cond::Version(row.version)),
                None => (0, Cond::Absent),
            };
            let start = stored.max(own);
            let end = start.saturating_add(ID_BLOCK);
            self.commit(Txn::new().put(counter, end.to_be_bytes(), cond))?;
            Ok(start..end)
        })
    }

    /// The id an operation is submitted under: `wanted` if it lies in the block this handle
    /// reserved, the start of that block if `wanted` is behind it (an earlier submit in
    /// the block failed, so the scheduler did not move on), else the start of a block
    /// reserved now (`wanted`, or the store's counter if that is later). The caller makes
    /// the scheduler number from the id returned.
    ///
    /// # Errors
    /// As [`Rows::reserve_seqs`].
    pub fn reserve_operation(&self, wanted: OperationId) -> Result<OperationId, StateError> {
        let mut ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        if ops.contains(&wanted.0) {
            return Ok(wanted);
        }
        if wanted.0 < ops.start {
            return Ok(OperationId(ops.start));
        }
        *ops = self.reserve(NEXT_OP, wanted.0)?;
        Ok(OperationId(ops.start))
    }

    /// Reserves a block of lease sequences from `own` on (or the store's counter, if that
    /// is later), for the scheduler to number leases in.
    ///
    /// # Errors
    /// The store cannot be read or written, the counter is not a number
    /// ([`StateError::Corrupt`]), or other frontends won every one of [`CAS_TRIES`] tries
    /// ([`StateError::Conflict`]).
    pub fn reserve_seqs(&self, own: u64) -> Result<Range<u64>, StateError> {
        self.reserve(NEXT_SEQ, own)
    }
}
