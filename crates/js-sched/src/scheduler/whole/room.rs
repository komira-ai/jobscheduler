//! Reads of the whole-node state by worker and by operation: which worker an operation
//! marks or holds ([`Scheduler::whole_marked_worker`]), which operation holds a worker
//! whole ([`Scheduler::whole_holder`]), and whether a worker can take a whole-machine
//! lease now ([`Scheduler::whole_room`]).
//!
//! As [`Scheduler::whole_state`] does, they read what the last placement round recorded
//! (the marks) and what the scheduler holds (leases, bars, workers), with the code
//! placement uses; the bars and whether a worker is live are read at the scheduler's time
//! now. They change nothing.

use std::collections::BTreeSet;

use js_types::sched::{LeaseKind, OperationId, WorkerId};

use crate::scheduler::Scheduler;
use crate::servable::{Servable, serves};

/// What a worker offers whole-machine requests now ([`Scheduler::whole_room`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Room {
    /// Whether a whole-machine lease could be placed on it by the facts placement reads:
    /// it is registered, heard from within the lease grace, serving and not cordoned, it
    /// serves whole-machine leases, and it holds no lease. Caps, marks and bars are not
    /// read here: [`Room::marked_for`] and [`Room::barred_for`] say who may take it.
    pub quiet: bool,
    /// The operations it is barred to now ([`Scheduler::whole_barred`]).
    pub barred_for: BTreeSet<OperationId>,
    /// The operation whose mark is on it, as of the last placement round
    /// ([`Scheduler::whole_mark`]): only that one is placed on it, but as backfill.
    pub marked_for: Option<OperationId>,
}

impl Scheduler {
    /// The worker whole-machine `operation` marks (as of the last placement round,
    /// [`Scheduler::whole_mark`]) or holds under its lease, leased or running. `None` for
    /// one that does neither, and for an operation that is not a whole-machine request or
    /// that the scheduler does not hold.
    #[must_use]
    pub fn whole_marked_worker(&self, operation: OperationId) -> Option<&WorkerId> {
        let held = || {
            let op = self.ops.get(&operation)?;
            let whole = op.request.kind == LeaseKind::WholeMachine;
            op.state.holding().filter(|_| whole).map(|(_, w)| w)
        };
        let marked = self.drain.marks.get(&operation).map(|m| &m.worker);
        marked.or_else(held)
    }

    /// The whole-machine operation whose lease, leased or running, holds `worker`; `None`
    /// when none does (an action that owns its worker is not one).
    #[must_use]
    pub fn whole_holder(&self, worker: &WorkerId) -> Option<OperationId> {
        self.held.values().map(|held| held.operation).find(|id| {
            let op = &self.ops[id];
            let on = op.state.holding().is_some_and(|(_, w)| w == worker);
            on && op.request.kind == LeaseKind::WholeMachine
        })
    }

    /// Whether `worker` can take a whole-machine lease now ([`Room::quiet`]), the
    /// operations barred from it, and the operation that marks it. A worker that has not
    /// registered is not quiet.
    #[must_use]
    pub fn whole_room(&self, worker: &WorkerId) -> Room {
        let live = Servable::new(&self.workers, &self.cordons, self.now).is_live(worker);
        let quiet = live
            && self
                .workers
                .get(worker)
                .is_some_and(|w| w.leases == 0 && serves(&w.caps, LeaseKind::WholeMachine));
        let barred_for = self.drain.barred_operations(worker, self.now).collect();
        let marked_for = self
            .drain
            .marks
            .iter()
            .find(|(_, mark)| &mark.worker == worker)
            .map(|(id, _)| *id);
        Room {
            quiet,
            barred_for,
            marked_for,
        }
    }
}
