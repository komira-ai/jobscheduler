//! The scheduler's rows in the state store: their keys, the blocks of ids a frontend
//! numbers with, the epoch it serves under, which frontend owns a daemon's node, a daemon's
//! session, how an operator placed a node, and the unfinished operations (their `op/`,
//! `queue/`, `inflight/` and `opkey/` rows, and the load of them at a start), over
//! [`js_state::Store`] and nothing else.
//!
//! The keys ([`keys`]) and the bytes of the `meta/`, `owner/`, `cordon/`, `queue/`,
//! `inflight/` and `opkey/` rows are those of the farm server's state store, byte for byte
//! (`tests/golden/`): a store written by either reads under the other, and rows only ever
//! gain new kinds, never new fields.
//!
//! [`Rows`] is a frontend's handle on a store. It takes time from the store's own clock and
//! does no I/O but through the store, so the crate is pure in the sense of `clippy.toml`.
//!
//! The bytes of the values of the `op/`, `lease/` and `session/` rows are not this crate's:
//! the embedding server supplies a [`RowCodec`]. The rows only that server has (what it
//! keeps with an operation, and with its result) are its [`Ext`]'s: this crate calls it
//! inside the transactions it builds and at a load, and sees what it keeps only as bytes.
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

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use js_state::{Store, Txn, Versioned};
use js_types::sched::WorkerId;

mod blocks;
mod codec;
mod cordon;
mod epoch;
mod error;
mod ext;
mod fate;
mod inflight;
pub mod keys;
mod leases;
mod load;
mod opkey;
mod owner;
mod session;
mod submit;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use blocks::{CAS_TRIES, ID_BLOCK};
pub use codec::{RowCodec, Value, WorkerUp};
pub use cordon::StoredCordon;
pub use epoch::{EpochBump, EpochPolicy};
pub use error::StateError;
pub use ext::Ext;
pub use load::{Stored, StoredOperation};
pub use owner::{Arrival, OWNER_CACHE_TTL, Owner};
pub use session::SESSION_RENEWAL;
pub use submit::Submitted;

use owner::Ownership;

/// A frontend's handle on its state store.
pub struct Rows {
    store: Arc<dyn Store>,
    /// What the values of the `op/`, `lease/` and `session/` rows are as bytes.
    codec: Arc<dyn RowCodec>,
    /// The rows only the embedding server has, beside the scheduler's.
    ext: Arc<dyn Ext>,
    /// Whether the `opkey/` rows are read ([`Rows::set_read_size_keys`]).
    size_keys: AtomicBool,
    /// The operation ids this handle reserved last ([`Rows::reserve_operation`]): numbered
    /// inside it, and no other frontend takes one of them.
    ops: Mutex<Range<u64>>,
    /// This frontend's id in the `owner/` rows it writes: the term it started under, which
    /// differs from every other frontend's but for a chance of about one in 2^16 within
    /// a millisecond. Ownership checks compare the counter too, so a shared id costs only
    /// the detection of a move between the two frontends.
    frontend: u64,
    /// The nodes this frontend claimed, and what it holds of each ([`Ownership`]).
    owners: Mutex<BTreeMap<WorkerId, Ownership>>,
    /// The owner each node's newest claim found, for the claims whose arrival did not
    /// complete (what the claim took over could not be read): the retry finds this
    /// frontend's own row, and takes the owner named here as the one it replaced.
    handovers: Mutex<BTreeMap<WorkerId, Option<Owner>>>,
    /// The value of the newest session row this handle wrote for each node, so a renewal
    /// that finds the row gone (it expired, or its write was refused) puts it again.
    sessions: Mutex<BTreeMap<WorkerId, Vec<u8>>>,
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
    /// Returns the handle and the epoch. `first` is the process term, which also names the
    /// handle's frontend in the `owner/` rows it writes: two frontends of a store start
    /// under one only by a chance of about one in 2^16 within a millisecond. `codec` is the
    /// encoding of the row values this crate does not own, and `ext` adds the embedding
    /// server's own rows to the transactions.
    ///
    /// # Errors
    /// The store cannot be read or written, its epoch row is not a number, or other
    /// frontends won every one of [`CAS_TRIES`] tries ([`StateError::Conflict`]).
    pub fn open(
        store: Arc<dyn Store>,
        policy: EpochPolicy,
        first: u64,
        codec: Arc<dyn RowCodec>,
        ext: Arc<dyn Ext>,
    ) -> Result<(Self, u64), StateError> {
        let rows = Self {
            store,
            codec,
            ext,
            size_keys: AtomicBool::new(true),
            ops: Mutex::new(0..0),
            frontend: first,
            owners: Mutex::new(BTreeMap::new()),
            handovers: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(BTreeMap::new()),
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
