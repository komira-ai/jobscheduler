//! The start time of every worker held in a placement round, carried from the pick that
//! held it into [`crate::servable::Servable::fit`], where later work is placed on it only
//! as backfill ([`crate::backfill`]).

use std::collections::{BTreeMap, BTreeSet};

use js_types::sched::{FarmTime, OperationId, WorkerId};
use js_whole::cost::DrainCost;

use super::Scheduler;
use crate::backfill::{self, Backfill};

/// The backfill setting and what it keeps between rounds.
#[derive(Clone, Debug, Default)]
pub(super) struct Gate {
    /// The setting ([`Scheduler::with_backfill`]).
    settings: Backfill,
    /// Workers held for requests this scheduler does not queue, with their start times
    /// ([`Scheduler::reserve_outside`]).
    outside: BTreeMap<WorkerId, Option<u64>>,
    /// The worker each operation held last round, with its start time.
    last: BTreeMap<OperationId, (WorkerId, Option<u64>)>,
    /// The workers an operation may be placed on, for an operation held to some
    /// ([`Scheduler::place_only_on`]).
    only: BTreeMap<OperationId, BTreeSet<WorkerId>>,
}

/// The holds of one placement round.
pub(super) struct Holds {
    /// The start time of each held worker, as farm milliseconds; what `fit` reads.
    pub(super) starts: BTreeMap<WorkerId, Option<u64>>,
    /// The worker each operation holds and its start time, kept for the next round.
    by_operation: BTreeMap<OperationId, (WorkerId, Option<u64>)>,
}

impl Gate {
    /// The start of a round: the workers held outside, as the round's first holds.
    pub(super) fn round(&self) -> (BTreeSet<WorkerId>, Holds) {
        let reserved = self.outside.keys().cloned().collect();
        let holds = Holds {
            starts: self.outside.clone(),
            by_operation: BTreeMap::new(),
        };
        (reserved, holds)
    }

    /// Records that `operation` holds `worker` this round, which the pick expects it to
    /// start on at `start`. A hold kept on the worker it held last round keeps the earlier
    /// start ([`backfill::kept`]): work placed since cannot move it later.
    pub(super) fn hold(
        &self,
        holds: &mut Holds,
        operation: OperationId,
        worker: &WorkerId,
        start: Option<u64>,
    ) {
        let start = match self.last.get(&operation) {
            Some((held, before)) if held == worker => backfill::kept(*before, start),
            _ => start,
        };
        holds.starts.insert(worker.clone(), start);
        holds
            .by_operation
            .insert(operation, (worker.clone(), start));
    }

    /// The workers `operation` may be placed on, when it is held to some.
    pub(super) fn only(&self, operation: OperationId) -> Option<&BTreeSet<WorkerId>> {
        self.only.get(&operation)
    }

    /// Forgets what is kept for `operation`, which is gone.
    pub(super) fn forget(&mut self, operation: OperationId) {
        self.only.remove(&operation);
    }

    /// The end of a round: its holds are the ones the next round keeps.
    pub(super) fn end_round(&mut self, holds: Holds) {
        self.last = holds.by_operation;
    }
}

/// The tail end `drain` gives `worker`: when a request that holds it is expected to start.
pub(super) fn tail_of(drain: &[DrainCost], worker: &WorkerId) -> Option<u64> {
    let cost = drain.iter().find(|cost| cost.id == worker.as_str());
    cost.and_then(|cost| cost.tail_end)
}

impl Scheduler {
    /// This scheduler, placing work on a held worker as `backfill` allows: a worker held
    /// for a whole-machine request or an action that owns its worker (with whole-node
    /// settings, [`Scheduler::with_whole_node`]), or for a blocked head, takes an operation
    /// after the holder in queue order only if the operation ends, with a minute to spare,
    /// by the holder's start time there. The start time is taken by the pick that holds the
    /// worker: the end of the worker's tail for a whole-machine request or an owner action,
    /// the first instant the head's charge fits for a head. A hold made again on the same
    /// worker keeps the earlier start, so work placed since never moves it later. A hold
    /// made without whole-node settings has no start time, and nothing backfills there.
    /// Work ahead of the holder in queue order is placed as before. The default
    /// ([`Backfill::default`]) places nothing on a held worker.
    #[must_use]
    pub fn with_backfill(mut self, backfill: Backfill) -> Self {
        self.backfill.settings = backfill;
        self
    }

    /// Holds `worker` for a request this scheduler does not queue, expected to start there
    /// at `start` (`None`: not known). From the next placement round no operation is
    /// placed on it, a probe pinned to it included, except as backfill that ends by
    /// `start` ([`Scheduler::with_backfill`]); and no request of this scheduler holds it.
    /// A second call for `worker` replaces its start. The lease server passes the hosts its
    /// registry has marked for whole-node requests this way, with each mark's tail end.
    pub fn reserve_outside(&mut self, worker: WorkerId, start: Option<FarmTime>) {
        let start = start.map(FarmTime::as_millis);
        self.backfill.outside.insert(worker, start);
    }

    /// Ends the hold [`Scheduler::reserve_outside`] put on `worker`: from the next round
    /// it takes work as any worker does. A worker not held outside is left as it is.
    pub fn release_outside(&mut self, worker: &WorkerId) {
        self.backfill.outside.remove(worker);
    }

    /// Holds `operation` to `workers`: from the next placement round it is placed on none
    /// but them, and on an empty set on none at all, whatever else would take it. A second
    /// call replaces the set. The set is dropped with the operation (retired or forgotten
    /// as queued). An operation never given a set is placed as before.
    ///
    /// The lease server's registry offers each start only the hosts eligible for it, and
    /// passes them this way; once the start is booked it passes an empty set, so the
    /// operation is never placed again by the scheduler alone.
    pub fn place_only_on(&mut self, operation: OperationId, workers: BTreeSet<WorkerId>) {
        self.backfill.only.insert(operation, workers);
    }

    /// How long queued `operation` counts for backfill ([`Backfill`]): its max wall time
    /// and the P90 of its key's wall minutes, as the setting reads them.
    pub(super) fn backfill_duration(&self, operation: OperationId) -> Option<u64> {
        let wall = self.max_walls.get(&operation).copied();
        let key = &self.ops[&operation].request.key;
        let p90 = || self.admission.wall_minutes_p90(key);
        self.backfill.settings.duration(wall, p90)
    }
}
