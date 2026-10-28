//! A daemon's `session/<worker>` row: written when it registers, renewed while it
//! heartbeats, and gone a lease grace after the last write.
//!
//! The row is one per session, however many leases the daemon holds. Its value is a
//! [`WorkerUp`], in the bytes of the [`RowCodec`](crate::RowCodec). Whether an operation is
//! unservable is judged from these rows, which hold every frontend's daemons, and not from
//! the daemons of one frontend.

use std::collections::BTreeSet;
use std::sync::PoisonError;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::Request;
use js_state::{Cond, Txn};
use js_types::sched::fence::LEASE_GRACE;
use js_types::sched::{Resources, WorkerId};

use crate::{Rows, StateError, WorkerUp, keys};

/// How often a daemon session's row in the store is renewed while the daemon heartbeats:
/// a third of the lease grace, so the row outlives two missed renewals. One write per
/// session per interval, whatever the number of leases it holds.
pub const SESSION_RENEWAL: Duration = Duration::from_secs(LEASE_GRACE.as_secs() / 3);

impl Rows {
    /// A daemon registered: its session row, as of the registration, expiring a lease
    /// grace after the store's clock.
    ///
    /// # Errors
    /// The registration cannot be encoded, or the store cannot be written.
    pub fn session(&self, up: &WorkerUp) -> Result<(), StateError> {
        let value = self.codec.session(up)?;
        let key = keys::session(&up.worker);
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.insert(up.worker.clone(), value.clone());
        drop(sessions);
        self.commit(Txn::new().put_ttl(key, value, LEASE_GRACE, Cond::Any))
    }

    /// Extends `worker`'s session row by the lease grace, from the store's clock. A
    /// daemon with no row (its registration was not written, or the row expired) gets the
    /// registration this handle wrote last put again, where there is still no row; a
    /// daemon this handle never wrote a registration for is left for its next
    /// registration to write.
    ///
    /// # Errors
    /// The store cannot be read or written, or the row changed under the renewal
    /// ([`StateError::Conflict`]).
    pub fn renew(&self, worker: &WorkerId) -> Result<(), StateError> {
        let key = keys::session(worker);
        let Some(row) = self.store.get(&key)? else {
            let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(value) = sessions.get(worker).cloned() else {
                return Ok(());
            };
            drop(sessions);
            return self.commit(Txn::new().put_ttl(key, value, LEASE_GRACE, Cond::Absent));
        };
        let renewed = Txn::new().put_ttl(key, row.value, LEASE_GRACE, Cond::Version(row.version));
        self.commit(renewed)
    }

    /// The capabilities and capacity of every daemon the session rows list that is not in
    /// `connected` (the nodes of this frontend): the daemons of the other frontends of the
    /// state store, each renewed by the frontend it is connected to.
    ///
    /// # Errors
    /// The store cannot be read, or a session row is not a daemon's registration.
    pub fn remote_sessions(
        &self,
        connected: &BTreeSet<WorkerId>,
    ) -> Result<Vec<(NodeCaps, Resources)>, StateError> {
        let mut remote = Vec::new();
        for (key, row) in self.store.scan_prefix(keys::SESSION)? {
            if keys::worker_of(keys::SESSION, &key).is_some_and(|node| connected.contains(&node)) {
                continue;
            }
            let up = self.session_row(&key, &row)?;
            remote.push((up.caps, up.capacity));
        }
        Ok(remote)
    }

    /// Whether a daemon session in the store, of any frontend, could run `request`: the
    /// session rows hold each connected daemon's capabilities and capacity, written by the
    /// frontend it is connected to, and expire unless that frontend renews them. An
    /// operation is unservable only if none could.
    ///
    /// # Errors
    /// The store cannot be read, or a session row is not a daemon's registration.
    pub fn session_could_run(&self, request: &Request) -> Result<bool, StateError> {
        for (key, row) in self.store.scan_prefix(keys::SESSION)? {
            let up = self.session_row(&key, &row)?;
            if request.runnable_on(&up.caps, &up.capacity) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
