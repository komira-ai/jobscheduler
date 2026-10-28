//! The `cordon/<worker>` rows: how an operator placed a node, kept until it is returned to
//! placement, so a successor process serves the node as the operator left it and the other
//! frontends of the store apply it at their next look at the rows.

use js_state::{Cond, Txn};
use js_types::sched::WorkerId;

use crate::{Rows, StateError, keys};

/// How an operator placed a node, as its `cordon/` row keeps it: a tag byte, and for a
/// drain the deadline as eight bytes, big-endian, milliseconds on the store's clock (a
/// frontend has a clock of its own for farm time, and the store's is the one all share; for
/// a store whose clock starts at the Unix epoch, the wall clock's own reading).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredCordon {
    /// Placement skips the node.
    Cordoned,
    /// Placement skips the node and waits for its leases until the deadline.
    Draining {
        /// When the drain ends, in milliseconds on the store's clock.
        deadline_unix_ms: u64,
    },
}

impl StoredCordon {
    fn to_row(self) -> Vec<u8> {
        match self {
            Self::Cordoned => vec![0],
            Self::Draining { deadline_unix_ms } => {
                [&[1][..], &deadline_unix_ms.to_be_bytes()].concat()
            }
        }
    }

    fn from_row(key: &str, row: &[u8]) -> Result<Self, StateError> {
        match row {
            [0] => Ok(Self::Cordoned),
            [1, deadline @ ..] => {
                let deadline = <[u8; 8]>::try_from(deadline)
                    .map_err(|_| StateError::corrupt(key, "a drain deadline is not 8 bytes"))?;
                Ok(Self::Draining {
                    deadline_unix_ms: u64::from_be_bytes(deadline),
                })
            }
            _ => Err(StateError::corrupt(key, "not a cordon")),
        }
    }
}

impl Rows {
    /// The nodes an operator placed, in node-name order.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not a cordon.
    pub fn cordons(&self) -> Result<Vec<(WorkerId, StoredCordon)>, StateError> {
        let rows = self.store.scan_prefix(keys::CORDON)?;
        rows.into_iter()
            .map(|(key, row)| {
                let cordon = StoredCordon::from_row(&key, &row.value)?;
                Ok((WorkerId::new(&key[keys::CORDON.len()..]), cordon))
            })
            .collect()
    }

    /// An operator placed `worker` as `cordon`, or returned it to placement (`None`).
    ///
    /// # Errors
    /// The store cannot be written.
    pub fn cordoned(
        &self,
        worker: &WorkerId,
        cordon: Option<StoredCordon>,
    ) -> Result<(), StateError> {
        let key = keys::cordon(worker);
        let txn = match cordon {
            Some(cordon) => Txn::new().put(key, cordon.to_row(), Cond::Any),
            None => Txn::new().delete(key, Cond::Any),
        };
        self.commit(txn)
    }
}
