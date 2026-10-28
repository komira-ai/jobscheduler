//! The scheduler's rows in the state store: their keys, the blocks of ids a frontend
//! numbers with, and the epoch it serves under, over [`js_state::Store`] and nothing else.
//!
//! The keys ([`keys`]) and the bytes of the `meta/` rows are those of the farm server's
//! state store, byte for byte (`tests/golden/`): a store written by either reads under the
//! other, and rows only ever gain new kinds, never new fields.
//!
//! [`Rows`] is a frontend's handle on a store. It takes time from the store's own clock and
//! does no I/O but through the store, so the crate is pure in the sense of `clippy.toml`.
//!
//! Several frontends may share one store. Each reserves a block of [`ID_BLOCK`] operation
//! ids or lease sequences with one compare-and-set write of `max(stored, its own next) +
//! ID_BLOCK` to `meta/next_op` or `meta/next_seq` (read again and tried again, at most
//! [`CAS_TRIES`] times, when another frontend wrote first) and numbers inside the block, so
//! the stored value never goes back and no two frontends, and no restarted one, take an id
//! twice. The `op/` and `lease/` rows of a finished operation are deleted, so their absence
//! guards no id. The value is eight bytes, big-endian. The epoch row is created with an
//! absent-only write, and a frontend that loses that race reads the other's.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use js_state::{Store, Txn, Versioned};

mod blocks;
mod epoch;
mod error;
pub mod keys;

pub use blocks::{CAS_TRIES, ID_BLOCK};
pub use epoch::{EpochBump, EpochPolicy};
pub use error::StateError;

/// A frontend's handle on its state store.
pub struct Rows {
    store: Arc<dyn Store>,
    /// The operation ids this handle reserved last ([`Rows::reserve_operation`]): numbered
    /// inside it, and no other frontend takes one of them.
    ops: Mutex<Range<u64>>,
}

impl fmt::Debug for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rows")
    }
}

impl Rows {
    /// Opens a handle on `store` and settles its epoch: a store that has none gets `first`
    /// (the process term, which no other store's epoch is likely to equal: a daemon must not
    /// mistake a new store for the one it knew), and `policy` decides for one that has.
    /// Returns the handle and the epoch.
    ///
    /// # Errors
    /// The store cannot be read or written, its epoch row is not a number, or other
    /// frontends won every one of [`CAS_TRIES`] tries ([`StateError::Conflict`]).
    pub fn open(
        store: Arc<dyn Store>,
        policy: EpochPolicy,
        first: u64,
    ) -> Result<(Self, u64), StateError> {
        let rows = Self {
            store,
            ops: Mutex::new(0..0),
        };
        let epoch = rows.settle_epoch(policy, first)?;
        Ok((rows, epoch))
    }

    /// The row at `key`, which the write that asks depends on.
    fn row(&self, key: &str) -> Result<Versioned, StateError> {
        let row = self.store.get(key)?;
        row.ok_or_else(|| StateError::Missing(key.to_owned()))
    }

    fn commit(&self, txn: Txn) -> Result<(), StateError> {
        self.store.txn(txn)??;
        Ok(())
    }
}

/// The number a counter or epoch row holds: eight bytes, big-endian.
fn number(key: &str, row: &Versioned) -> Result<u64, StateError> {
    let bytes = <[u8; 8]>::try_from(row.value.as_slice())
        .map_err(|_| StateError::corrupt(key, "not an 8-byte number"))?;
    Ok(u64::from_be_bytes(bytes))
}
