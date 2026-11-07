//! What the engine keeps of each lease of the registry it holds, beside the scheduler: when
//! it started, the longest it may run, and whether it is a whole-node lease.
//!
//! The scheduler takes a lease it takes up (one put back after a restart, one the registry
//! booked without the engine placing it, every lease of an engine built again) as started at
//! the take-up, with no wall time, and as an action: its drain cost would read the lease as
//! ending at the latest time there is, counted from the take-up, and a whole-node request
//! could mark the host a whole-node lease holds. So the engine keeps the three of every lease
//! it places or takes up, and gives them to the scheduler with the lease
//! ([`Engine::keep`]): at a start that places it, at a restart's put-back
//! ([`Engine::seat`], from the start and the end bound the lease file records; a put-back
//! with neither ends at once, as the registry counts it), and at each
//! rebuild, from what the engine before it kept ([`Engine::rebuild`]). A lease the registry
//! stops is forgotten ([`Engine::ended`]).

use std::time::Duration;

use js_registry::SeatedLease;
use js_types::LeaseId;
use js_types::sched::FarmTime;

use super::{Engine, Placed};

/// What the engine keeps of one lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    /// When it started; `None` when that is not known, and the scheduler counts from when it
    /// took the lease up.
    pub start: Option<FarmTime>,
    /// The longest it may run from its start.
    pub max_wall: Duration,
    /// Whether it is a whole-node lease, which holds its host whole.
    pub whole_node: bool,
}

impl Kept {
    /// A lease started at `start` that may run `max_wall`, a whole-node lease when
    /// `whole_node`.
    pub(crate) fn started(start: FarmTime, max_wall: Duration, whole_node: bool) -> Self {
        Self {
            start: Some(start),
            max_wall,
            whole_node,
        }
    }

    /// What `seated` says of a lease put back: its start, and the time from there to its end
    /// bound, as the lease file records them (no time when the end is before the start).
    /// Put back with no times, it counts as ending at the put-back, as the registry counts
    /// such a lease.
    pub(crate) fn of_seated(seated: &SeatedLease) -> Self {
        let times = seated.times;
        let wall = times.map_or(0, |times| {
            let started = times.started_at.millis();
            times.ends_at.millis().saturating_sub(started)
        });
        Self {
            start: times.map(|times| FarmTime::from_millis(times.started_at.millis())),
            max_wall: Duration::from_millis(wall),
            whole_node: seated.whole_node,
        }
    }
}

impl Engine {
    /// Takes up `seated`'s lease, which the registry put back on its host at a start: held
    /// there as a lease recovered after a restart is, booking what the registry books, and
    /// counted from the start and bound the lease file records. A lease the engine holds
    /// already is left as it is.
    pub fn seat(&mut self, seated: &SeatedLease) {
        let lease = seated.held.lease;
        if self.on.values().any(|leases| leases.contains_key(&lease)) {
            return;
        }
        let placed = self.take_up(&seated.host, &seated.held);
        self.keep(lease, placed, Kept::of_seated(seated));
    }

    /// Gives the scheduler what is known of `lease`, which the engine holds as `placed`, and
    /// keeps it for a rebuild.
    pub(super) fn keep(&mut self, lease: LeaseId, placed: Placed, kept: Kept) {
        self.sched.set_max_wall(placed.operation, kept.max_wall);
        if let Some(start) = kept.start {
            self.sched.restore_start(placed.lease, start);
        }
        if kept.whole_node {
            self.sched.set_whole_holder(placed.operation);
        }
        self.kept.insert(lease, kept);
    }

    /// Gives this engine, just built, what `old` kept of each lease this one holds.
    pub(super) fn carry_kept(&mut self, old: &Self) {
        let held: Vec<(LeaseId, Placed)> = self
            .on
            .values()
            .flat_map(|leases| leases.iter().map(|(lease, placed)| (*lease, *placed)))
            .collect();
        for (lease, placed) in held {
            if let Some(kept) = old.kept.get(&lease) {
                self.keep(lease, placed, *kept);
            }
        }
    }

    /// What the engine keeps of `lease`, for a test that reads it.
    #[cfg(test)]
    pub(crate) fn kept(&self, lease: &LeaseId) -> Option<Kept> {
        self.kept.get(lease).copied()
    }
}

#[cfg(test)]
#[path = "kept_tests.rs"]
mod tests;
