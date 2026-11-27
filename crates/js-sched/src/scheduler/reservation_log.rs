//! Keeping the reservation records ([`crate::reservation`]): which large requests are
//! blocked, the worker each holds and the start expected there round by round, and what
//! each lease's placement expected of it.
//!
//! A request is tracked from the first placement round it is blocked as a large request
//! ([`Log::blocked`], or the first round it holds a worker) until it leaves the queue,
//! when its record is emitted ([`Scheduler::reservation_ended`]). Each round's holds
//! ([`Log::held`]) give its first and last expected start and count its moves. All of it
//! is kept only while the scheduler records ([`Scheduler::recording_reservations`]), and
//! nothing the scheduler decides reads it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_types::sched::{FarmTime, OperationId, WorkerId};

use super::Scheduler;
use super::gate::Holds;
use crate::reservation::{LeaseRelease, ReservationEnd, ReservationEvent, ReservationRecord};
use crate::servable::{Attempt, Placed};

/// What a lease's placement expected of it, kept with the lease until it ends.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Placement {
    /// How long it was expected to run, in milliseconds ([`crate::Backfill`]).
    estimate: Option<u64>,
    /// Whether it went on a held worker, as backfill.
    backfilled: bool,
    /// The request of this scheduler that held that worker (`None` for a worker held
    /// from outside, or for a lease not backfilled).
    holder: Option<OperationId>,
}

impl Placed {
    /// This placement, with what `attempt` expected of it in a round whose holds so far
    /// are `holds`: a lease on a worker `attempt` saw held went there as backfill.
    pub(super) fn noted(mut self, attempt: &Attempt<'_>, holds: &Holds) -> Self {
        let backfilled = attempt.reserved.contains(&self.worker);
        self.placement = Placement {
            estimate: attempt.backfill,
            backfilled,
            holder: holds.holder(&self.worker).filter(|_| backfilled),
        };
        self
    }
}

/// The records not yet taken, and the large requests still queued.
#[derive(Clone, Debug, Default)]
pub(super) struct Log {
    /// Every large request still queued, by request.
    open: BTreeMap<OperationId, Open>,
    /// What happened since the caller last took it, oldest first.
    events: Vec<ReservationEvent>,
}

/// A large request still queued: its record so far.
#[derive(Clone, Debug)]
pub(super) struct Open {
    blocked_at: FarmTime,
    reserved_at: Option<FarmTime>,
    estimated_start: Option<FarmTime>,
    last_estimate: Option<FarmTime>,
    /// The worker it held last, in any round.
    worker: Option<WorkerId>,
    moves: u32,
    /// Backfill on its worker, in thousandths of a core times milliseconds, from the
    /// leases already ended.
    backfilled: u64,
}

impl Log {
    /// `operation` is blocked as a large request in the round at `now`: tracked from now,
    /// if it is not yet.
    pub(super) fn blocked(&mut self, operation: OperationId, now: FarmTime) -> &mut Open {
        self.open.entry(operation).or_insert_with(|| Open {
            blocked_at: now,
            reserved_at: None,
            estimated_start: None,
            last_estimate: None,
            worker: None,
            moves: 0,
            backfilled: 0,
        })
    }

    /// The holds of the round at `now`, each request's worker and expected start (farm
    /// milliseconds): the first one a request has is its reservation, one on another
    /// worker than its last is a move, and each is its last estimate.
    pub(super) fn held(
        &mut self,
        now: FarmTime,
        holds: &BTreeMap<OperationId, (WorkerId, Option<u64>)>,
    ) {
        for (&operation, (worker, start)) in holds {
            let open = self.blocked(operation, now);
            let start = start.map(FarmTime::from_millis);
            match &open.worker {
                None => {
                    open.reserved_at = Some(now);
                    open.estimated_start = start;
                }
                Some(last) if last != worker => open.moves += 1,
                Some(_) => {}
            }
            open.worker = Some(worker.clone());
            open.last_estimate = start;
        }
    }
}

/// Thousandths of a core times milliseconds that `cpu_millis` booked from `from` to
/// `to` come to.
fn core_millis(cpu_millis: u64, from: FarmTime, to: FarmTime) -> u64 {
    let ms = to.as_millis().saturating_sub(from.as_millis());
    cpu_millis.saturating_mul(ms)
}

impl Scheduler {
    /// This scheduler, keeping a [`ReservationEvent`] for each large request that leaves
    /// the queue and each lease that ends, until [`Scheduler::take_reservations`] takes
    /// them. Off by default, so a caller that never takes them does not collect them
    /// without end.
    #[must_use]
    pub fn recording_reservations(mut self) -> Self {
        self.backfill.log = Some(Log::default());
        self
    }

    /// What happened to reservations and leases since the last call, oldest first;
    /// always empty unless built with [`Scheduler::recording_reservations`].
    pub fn take_reservations(&mut self) -> Vec<ReservationEvent> {
        self.backfill
            .log
            .as_mut()
            .map(|log| std::mem::take(&mut log.events))
            .unwrap_or_default()
    }

    /// `operation` left the queue as `end` says: emits its record if it is a tracked large
    /// request, counting the backfill still running on its worker up to now. That backfill
    /// is no longer counted to it: a later reservation of the same request (its lease given
    /// up, the request blocked again) counts only the backfill placed around that one.
    pub(super) fn reservation_ended(
        &mut self,
        operation: OperationId,
        end: impl FnOnce() -> ReservationEnd,
    ) {
        let Some(log) = &mut self.backfill.log else {
            return;
        };
        let Some(open) = log.open.remove(&operation) else {
            return;
        };
        let now = self.now;
        let mut running = 0u64;
        let around = self.held.values_mut();
        for held in around.filter(|held| held.placement.holder == Some(operation)) {
            held.placement.holder = None;
            let used = core_millis(held.booked.cpu_millis, held.granted_at, now);
            running = running.saturating_add(used);
        }
        let backfilled = open.backfilled.saturating_add(running);
        log.events.push(ReservationEvent::Ended(ReservationRecord {
            operation,
            blocked_at: open.blocked_at,
            reserved_at: open.reserved_at,
            estimated_start: open.estimated_start,
            last_estimate: open.last_estimate,
            moves: open.moves,
            backfilled_core_seconds: backfilled / 1_000_000,
            ended_at: now,
            end: end(),
        }));
    }

    /// The lease `operation` holds is about to end: records its release, and adds its
    /// backfill to the request whose worker it was placed on, if that one still waits.
    pub(super) fn record_release(&mut self, operation: OperationId) {
        let Some(log) = &mut self.backfill.log else {
            return;
        };
        let now = self.now;
        let holding = self.ops[&operation].state.holding();
        let ended = holding
            .and_then(|(lease, worker)| self.held.get(&lease).map(|held| (lease, worker, held)));
        let Some((lease, worker, held)) = ended else {
            return;
        };
        let placement = held.placement;
        let holder = placement
            .holder
            .and_then(|holder| log.open.get_mut(&holder));
        if let Some(open) = holder {
            let used = core_millis(held.booked.cpu_millis, held.granted_at, now);
            open.backfilled = open.backfilled.saturating_add(used);
        }
        log.events.push(ReservationEvent::Released(LeaseRelease {
            lease,
            operation,
            worker: worker.clone(),
            granted_at: held.granted_at,
            released_at: now,
            estimate: placement.estimate.map(Duration::from_millis),
            backfilled: placement.backfilled,
        }));
    }
}
