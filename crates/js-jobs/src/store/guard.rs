//! The guarded views of a job store: every write one makes is conditional on a host's node row,
//! either still being at the version given ([`JobStore::guarded`]), so a beat's listing writes
//! stop once a newer beat or hello of the host has been applied (design `multi-instance.md`,
//! section 0 and G15), or still being absent ([`JobStore::guarded_absent`]), so the settlement
//! of a removed host loses nothing once a hello of the host has written its row again (G6).

use js_state::{Cond, Conflict, Store, Txn, Version};
use js_types::HostId;

use super::JobStore;
use crate::error::JobStoreError;
use crate::keys;

/// The node row a guarded view's writes are conditional on.
#[derive(Debug, Clone)]
pub(super) struct Guard {
    /// The host whose row it is.
    host: HostId,
    /// The version the row must be at, or `None` for a row that must be absent.
    at: Option<Version>,
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
        self.guarded_at(host, Some(at))
    }

    /// A job store as [`JobStore::guarded`] makes one, whose every write checks instead that
    /// `host` has no node row: the view the job pass loses a removed host's allocations through
    /// (design `multi-instance.md`, G6). A write that finds a row there (a hello of the host
    /// wrote one since the pass read it absent) is [`JobStoreError::NodeMoved`] and writes
    /// nothing. Reads are not guarded; guarding a view that is already guarded replaces its
    /// guard.
    #[must_use]
    pub fn guarded_absent(&self, host: &HostId) -> JobStore<S, I> {
        self.guarded_at(host, None)
    }

    /// The view whose every write checks that `host`'s node row is at `at` (absent for `None`).
    fn guarded_at(&self, host: &HostId, at: Option<Version>) -> JobStore<S, I> {
        JobStore {
            store: self.store.clone(),
            ids: self.ids.clone(),
            schema: self.schema,
            instance: self.instance,
            writes: self.writes.clone(),
            heads: self.heads.clone(),
            unclean: self.unclean.clone(),
            guard: Some(Guard {
                host: host.clone(),
                at,
            }),
            retention: self.retention,
            allocation_retention: self.allocation_retention,
            orphans: self.orphans.clone(),
        }
    }
}

impl<S: Store, I> JobStore<S, I> {
    /// `txn` with the check of the guard's node row, or `txn` itself for a job store that is
    /// not guarded.
    pub(super) fn guard_txn(&self, txn: Txn) -> Txn {
        match &self.guard {
            Some(guard) => {
                let cond = guard.at.map_or(Cond::Absent, Cond::Version);
                txn.check(keys::node(&guard.host), cond)
            }
            None => txn,
        }
    }

    /// `Ok(conflict)` for a conflict the caller answers; [`JobStoreError::NodeMoved`] when the
    /// guard's row has moved. A transaction that conflicts on several keys reports one of them,
    /// which need not be the guard's: a newer beat that committed the same job change as this
    /// one moves both that job's row and the node row. So a guarded view that loses any write
    /// reads the node row again, and a row no longer at the guard's version (or gone, or for a
    /// view guarded on the row's absence, there) is the move, whatever key the store named.
    ///
    /// # Errors
    /// [`JobStoreError::NodeMoved`]; [`JobStoreError::Store`] when the node row cannot be read.
    pub(super) fn refuse_moved(&self, conflict: Conflict) -> Result<Conflict, JobStoreError> {
        let Some(guard) = &self.guard else {
            return Ok(conflict);
        };
        let key = keys::node(&guard.host);
        let on_guard = conflict.key == key;
        let moved = on_guard || self.store.get(&key)?.map(|row| row.version) != guard.at;
        match moved {
            true => Err(JobStoreError::NodeMoved {
                host: guard.host.clone(),
            }),
            false => Ok(conflict),
        }
    }
}
