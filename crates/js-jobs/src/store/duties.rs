//! The periodic duties of the servers over one store (expiry, retention, settlement, the feed's
//! trim): each is a row, `periodic/<duty>`, holding when it next runs, and whichever server
//! advances it runs the duty for that period.

use std::time::Duration;

use js_state::{Cond, Store, StoreTime, Txn};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::IdSource;
use crate::keys;

/// The time a duty's row holds, or `None` for a value that is not one (a row of another build,
/// which is as good as due and is written over).
fn due_at(value: &[u8]) -> Option<StoreTime> {
    let micros = u64::from_be_bytes(value.try_into().ok()?);
    Some(StoreTime::from_duration(Duration::from_micros(micros)))
}

/// The bytes of the row for the time `at`.
fn encode(at: StoreTime) -> [u8; 8] {
    u64::try_from(at.as_duration().as_micros())
        .unwrap_or(u64::MAX)
        .to_be_bytes()
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Claims the periodic duty `duty` for the period that begins now by the store's clock
    /// ([`Store::now`], read once by the call: the row's due time is shared by every server, so
    /// no server's own clock judges it): `true` when this call advanced the duty's row to now
    /// plus `period`, so the caller runs the duty, and `false`
    /// when the duty is not due yet or another server advanced the row first (the row is
    /// advanced with a compare-and-set, so of any number of servers that find it due, one
    /// commits). A server that dies after claiming costs the duty one period, and the next
    /// server that finds it due runs it. Every duty is written to be run again and cut off at
    /// any point, so a claim is for economy, never for safety.
    ///
    /// A `period` of zero claims every call and writes nothing: no server is made to wait for
    /// another.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store fails, its clock included; nothing is written,
    /// and the caller does not run the duty then.
    pub fn claim_duty(&self, duty: &str, period: Duration) -> Result<bool, JobStoreError> {
        if period.is_zero() {
            return Ok(true);
        }
        let now = self.store.now()?;
        let key = keys::duty(duty);
        let cond = match self.store.get(&key)? {
            None => Cond::Absent,
            Some(row) => {
                if due_at(&row.value).is_some_and(|due| now < due) {
                    return Ok(false);
                }
                Cond::Version(row.version)
            }
        };
        let next = encode(now.saturating_add(period));
        Ok(self
            .write(Txn::new().put(key, next.to_vec(), cond))?
            .is_ok())
    }
}
