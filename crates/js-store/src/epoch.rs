//! The epoch row: made once, kept by a start and raised by the operator.

use js_state::{Cond, Txn};

use crate::blocks::contended;
use crate::keys::EPOCH;
use crate::{Rows, StateError, number};

/// What a start does with the store's epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochPolicy {
    /// Keep it: the scheduler resumes the store's incarnation, and a daemon that
    /// reconnects keeps the runs of its leases.
    Keep,
    /// Take the next one: the start is a new incarnation, and a daemon that sees it
    /// kills the runs of the leases it held before.
    Bump,
}

/// An operator's raise of the stored epoch ([`Rows::bump_epoch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochBump {
    /// The epoch the store held before.
    pub previous: u64,
    /// The epoch it holds now.
    pub epoch: u64,
}

impl Rows {
    /// The epoch this start runs under. A store with no epoch row gets one made with an
    /// absent-only write; if another frontend made it first, the write conflicts and the
    /// row is read again, so several frontends that open an empty store together end with
    /// the one epoch. A bump that loses to another writer reads the new row and bumps it.
    pub(crate) fn settle_epoch(&self, policy: EpochPolicy, first: u64) -> Result<u64, StateError> {
        contended(|| {
            let row = self.store.get(EPOCH)?;
            let (epoch, write) = match (&row, policy) {
                (None, _) => (first, Some(Cond::Absent)),
                (Some(row), EpochPolicy::Keep) => (number(EPOCH, row)?, None),
                (Some(row), EpochPolicy::Bump) => {
                    (number(EPOCH, row)? + 1, Some(Cond::Version(row.version)))
                }
            };
            if let Some(cond) = write {
                self.commit(Txn::new().put(EPOCH, epoch.to_be_bytes(), cond))?;
            }
            Ok(epoch)
        })
    }

    /// Raises the stored epoch by one: an operator's bump, for the day the store was
    /// restored from a backup and the daemons must drop the leases the lost history
    /// granted. A frontend reads the epoch when it starts, so one that runs now keeps the
    /// epoch it started with. Returns the epoch before and the one stored now. A bump
    /// that loses to another writer reads the new row and bumps that.
    ///
    /// # Errors
    /// The store cannot be read or written, there is no epoch row ([`StateError::Missing`])
    /// or it is not a number ([`StateError::Corrupt`]), or other writers won every one of
    /// [`crate::CAS_TRIES`] tries ([`StateError::Conflict`]).
    pub fn bump_epoch(&self) -> Result<EpochBump, StateError> {
        contended(|| {
            let row = self.row(EPOCH)?;
            let previous = number(EPOCH, &row)?;
            let epoch = previous + 1;
            let cond = Cond::Version(row.version);
            self.commit(Txn::new().put(EPOCH, epoch.to_be_bytes(), cond))?;
            Ok(EpochBump { previous, epoch })
        })
    }
}
