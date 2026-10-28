//! The orphan sweep: a lease whose node no frontend hears is requeued.
//!
//! A lease is held by a daemon connected to one frontend of the state store, and the store's
//! `lease/` row is the frontend's. If the frontend dies with its daemons (the host fails),
//! nothing gives the lease up: the frontends that remain hold no state for it. Every frontend
//! therefore looks, at each intake poll, for leases whose node has no `session/` row: the rows
//! expire [`LEASE_GRACE`] after the last write of the frontend that owns the node, which writes
//! one every [`SESSION_RENEWAL`] while the daemon heartbeats, and no frontend writes a row for
//! a node it does not hear. A lease whose node is connected to this frontend is left alone,
//! whatever its session row says.
//!
//! The first look that finds such a lease remembers it ([`Orphans`]), with the store's clock
//! reading, the version of its row and that of its node's `owner/` row. A later look that finds
//! the lease as it was, and its node still without a session, [`ORPHAN_WAIT`] after the first
//! (by two readings of the store's clock, never this host's), requeues it in one transaction
//! that checks the rows it read: the `op/` row, the `owner/` row at its version (a daemon that
//! said `Hello` to a frontend since has a session there within moments, and owns its lease
//! again), and deletes the `lease/` row at its version and its `lease-by-op/` row, and writes
//! the loss count one higher, as any lease lost to a silent node is counted, and the `queue/`
//! row where there is none. A lease that changed, or whose node came back, starts the wait
//! over; so does one whose requeue lost to a write of another frontend between the look and the
//! transaction, and the pass goes on to the other leases.
//!
//! Why the wait is safe. A daemon stops its work `SELF_FENCE` after the newest heartbeat its
//! frontend acknowledged, and a frontend acknowledges only while it wrote the node's session
//! row within [`ACK_WINDOW`], so the daemon runs no longer than `ACK_WINDOW` plus `SELF_FENCE`
//! (and the margin the fence keeps) past the last write of the row. The row was there
//! `LEASE_GRACE` after that write, so the first look that misses it is made at least that long
//! after the write, and the second at least [`ORPHAN_WAIT`] after the first: the check below
//! holds at compile time.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_state::{Cond, StoreTime, Txn, Version, Versioned};
use js_types::sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE};
use js_types::sched::{LeaseGrant, LeaseId, WorkerId};

use crate::{ACK_WINDOW, Rows, SESSION_RENEWAL, StateError, keys, number};

/// How long a lease whose node has no session row is left, measured between two readings of
/// the store's clock, before it is requeued: one renewal interval of the session row (the
/// first look that misses the row is made after its expiry) and the margin of the fence.
pub const ORPHAN_WAIT: Duration = SESSION_RENEWAL.saturating_add(LEADER_LEASE_MARGIN);

// The daemon of a lease has stopped its work by the second look: it ran at most `ACK_WINDOW`
// and `SELF_FENCE` (and the margin) past the last write of its session row, which was
// `LEASE_GRACE` before the first look at the latest.
const _: () = assert!(
    LEASE_GRACE.as_millis() + ORPHAN_WAIT.as_millis()
        > ACK_WINDOW.as_millis() + SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis()
);

/// A lease whose node had no session row at a look.
#[derive(Clone, Copy, Debug)]
struct Look {
    /// The store's clock at the look.
    first: StoreTime,
    /// The version of the lease's row.
    lease: Version,
    /// The version of its node's `owner/` row, if the node had one.
    owner: Option<Version>,
}

/// The leases a frontend has found without a heard node, between its looks ([`Rows::sweep`]).
#[derive(Debug, Default)]
pub struct Orphans(BTreeMap<LeaseId, Look>);

impl Orphans {
    /// How many leases are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no lease is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Rows {
    /// One look at the leases whose node has no session row: remembers each in `seen` (and
    /// forgets those that are none now), and requeues those `wait` after the first look that
    /// found them as they are. Leases of the nodes in `connected` are left alone.
    ///
    /// # Errors
    /// The store cannot be read or written, or a row read is not one the scheduler wrote.
    pub fn sweep(
        &self,
        wait: Duration,
        connected: &BTreeSet<WorkerId>,
        seen: &mut Orphans,
    ) -> Result<(), StateError> {
        let now = self.store.now();
        let sessions: BTreeSet<WorkerId> = self
            .store
            .scan_prefix(keys::SESSION)?
            .into_iter()
            .filter_map(|(key, _)| keys::worker_of(keys::SESSION, &key))
            .collect();
        let mut orphaned = BTreeSet::new();
        for (key, row) in self.store.scan_prefix(keys::LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            if connected.contains(&grant.worker) || sessions.contains(&grant.worker) {
                continue;
            }
            orphaned.insert(grant.lease);
            let owner = self
                .store
                .get(&keys::owner(&grant.worker))?
                .map(|owner| owner.version);
            let look = Look {
                first: now,
                lease: row.version,
                owner,
            };
            match seen.0.entry(grant.lease) {
                Entry::Vacant(slot) => {
                    slot.insert(look);
                }
                Entry::Occupied(mut slot) => {
                    let first = *slot.get();
                    if first.lease != look.lease || first.owner != look.owner {
                        slot.insert(look);
                    } else if now.since(first.first) >= wait {
                        // A row changed since the look: the lease is looked at afresh by the
                        // next pass, and the pass goes on to the other leases.
                        match self.requeue_orphan(&grant, &row, owner) {
                            Ok(()) | Err(StateError::Conflict(_)) => {
                                slot.remove();
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
        seen.0.retain(|lease, _| orphaned.contains(lease));
        Ok(())
    }

    /// Requeues the lease `lease` (`grant`), whose node's `owner/` row is at `owner`: one
    /// transaction that deletes the lease and its index row, queues the operation and counts
    /// the loss, if the rows are as read. An operation with no `op/` row has finished and has
    /// nothing to queue.
    fn requeue_orphan(
        &self,
        grant: &LeaseGrant,
        lease: &Versioned,
        owner: Option<Version>,
    ) -> Result<(), StateError> {
        let op = keys::operation(grant.operation);
        let Some(unfinished) = self.store.get(&op)? else {
            return Ok(());
        };
        let losses = keys::loss(grant.operation);
        let (count, cond) = match self.store.get(&losses)? {
            Some(row) => (
                number(&losses, &row)?.saturating_add(1),
                Cond::Version(row.version),
            ),
            None => (1, Cond::Absent),
        };
        let txn = Txn::new()
            .check(op, Cond::Version(unfinished.version))
            .check(
                keys::owner(&grant.worker),
                owner.map_or(Cond::Absent, Cond::Version),
            )
            .delete(keys::lease(grant.lease), Cond::Version(lease.version))
            .delete(keys::lease_by_op(grant.operation), Cond::Any)
            .put(losses, count.to_be_bytes(), cond)
            .put(keys::queue(grant.operation), Vec::new(), Cond::Absent);
        self.commit(txn)?;
        tracing::warn!(
            operation = %grant.operation,
            lease = %grant.lease,
            node = %grant.worker,
            "no frontend hears the node of the lease: the operation is queued again"
        );
        Ok(())
    }
}
