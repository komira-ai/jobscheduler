//! The guarded view of a job store ([`JobStore::guarded`]): every write it makes is conditional
//! on a host's node row still being at the version given, so a beat's listing writes stop once
//! a newer beat or hello of the host has been applied (design `multi-instance.md`, section 0
//! and G15).

use js_state::{Cond, Conflict, Txn, Version};
use js_types::HostId;

use super::JobStore;
use crate::error::JobStoreError;
use crate::keys;

/// The node row a guarded view's writes are conditional on.
#[derive(Debug, Clone)]
pub(super) struct Guard {
    /// The host whose row it is.
    host: HostId,
    /// The version the row must be at.
    at: Version,
}

impl<S, I: Clone> JobStore<S, I> {
    /// A job store over the same store, ids, schema and retention, and the same instance number
    /// and write count (so [`JobStore::written`] readings of the two agree), whose every
    /// write also checks that the node row of `host` is at `at`. A write that finds the row
    /// moved (a newer hello or beat of the host was applied) is [`JobStoreError::NodeMoved`]
    /// and writes nothing; this covers [`JobStore::place_claimed`] too, whose commit carries
    /// the check. Reads are not guarded. Guarding a view that is already guarded replaces its
    /// guard.
    #[must_use]
    pub fn guarded(&self, host: &HostId, at: Version) -> JobStore<S, I> {
        JobStore {
            store: self.store.clone(),
            ids: self.ids.clone(),
            schema: self.schema,
            instance: self.instance,
            writes: self.writes.clone(),
            guard: Some(Guard {
                host: host.clone(),
                at,
            }),
            retention: self.retention,
            allocation_retention: self.allocation_retention,
        }
    }
}

impl<S, I> JobStore<S, I> {
    /// `txn` with the check of the guard's node row, or `txn` itself for a job store that is
    /// not guarded.
    pub(super) fn guard_txn(&self, txn: Txn) -> Txn {
        match &self.guard {
            Some(guard) => txn.check(keys::node(&guard.host), Cond::Version(guard.at)),
            None => txn,
        }
    }

    /// `Ok(conflict)` for a conflict that is not on the guard's row, for the caller to answer;
    /// [`JobStoreError::NodeMoved`] for one that is.
    pub(super) fn refuse_moved(&self, conflict: Conflict) -> Result<Conflict, JobStoreError> {
        match &self.guard {
            Some(guard) if conflict.key == keys::node(&guard.host) => {
                Err(JobStoreError::NodeMoved {
                    host: guard.host.clone(),
                })
            }
            _ => Ok(conflict),
        }
    }
}
