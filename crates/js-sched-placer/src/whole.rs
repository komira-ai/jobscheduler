//! The whole-node queue the engine keeps for the registry under `--placement js-sched`
//! ([`Engine::whole_round`]): the engine, not the registry's own pass, marks, commits and
//! cancels the pool's whole-node requests.
//!
//! At each pass of the queue (each tick and each start) the registry hands over
//! [`WholeRound`]: its wait list (first presentation first), how it stands each host, the
//! hosts held for requests not presented since a restart, and the expected life of each
//! lease. While no request waits and the engine queues none, the round is not taken: the
//! engine lets go of the cordons and phases it set for the queue and places as it does for a
//! pool with no whole-node request. Otherwise it:
//!
//! 1. Takes the pool's whole-node settings ([`Scheduler::with_whole_node`]) and the lives,
//!    as learned curves under the work key of each lease ([`crate::lives::Lives`]), so the
//!    drain cost reads the registry's tails.
//! 2. Cancels the request of every operation whose request has left the wait list
//!    (cancelled, a `DELETE` of its pending lease, not presented within `regain_grace`):
//!    [`Scheduler::cancel_whole`] drops its mark, or its pending grant, at once.
//! 3. Follows how the registry stands each host: a tainted host is quarantined and a
//!    leaving one draining in the engine, and a lost one is cordoned, so none is marked or
//!    granted, a mark on it moves and a pending grant there is put back. A host the
//!    registry would not book now and that carries nothing (restoring after a whole-node
//!    lease, a run still listed, cordoned by an operator) is cordoned too while an idle
//!    host the registry would book is free, unless one of the engine's requests holds it:
//!    a request takes the bookable host first, a mark or pending grant already on it
//!    stays, and with no bookable host free a request is granted it, to wait there.
//! 4. Holds the hosts of [`WholeRound::outside`] outside its queue
//!    ([`Scheduler::reserve_outside`]), with their tails, for backfill.
//! 5. Queues each newly presented request as a whole-machine operation, in the order of the
//!    wait list, held to the time it was first presented, with the give-ups it has and the
//!    mark the registry holds for it ([`Scheduler::restore_whole`]); and bars every request
//!    from the hosts its failed prepares left out ([`Scheduler::bar_whole`]).
//! 6. Runs one placement round. A request granted a quiet worker holds a pending lease
//!    there: the registry shows it as an assigned reservation, and the request's next start
//!    commits on that host ([`Engine::commit_whole`]). A request that fits nowhere marks the
//!    worker cheapest to drain once it has waited `drain_after`, and keeps it until it is
//!    placed, cancelled or gives it up at its deadline.
//!
//! A pending grant is a held whole worker to the scheduler, and its drop at the start of a
//! round never touches one; the registry drops its newest reservations, an assigned one
//! included, when the ceiling falls. So before each round the engine puts back the newest
//! pending grants beyond the ceiling ([`Engine::whole_tick`]); a committed lease is never
//! touched. The holds a request gives up at their deadlines are counted, and the registry
//! keeps that count in its record of the request ([`Engine::whole_give_ups`]).
//!
//! The leases of whole-node requests are numbered in their own term ([`WHOLE_TERM`]), at
//! a round of their own before any other is placed ([`Engine::whole_tick`]): a round that
//! places a start or a job numbers its lease as the registry or the job table does, and a
//! whole-node request granted there instead is put back ([`Engine::settle_whole`]). Once
//! committed, the lease is the registry's: it is held as any lease is and ended with
//! [`Scheduler::cancel_whole`].
//!
//! A rebuild of the engine keeps the queue: the last round is handed to the new engine with
//! the marks, give-ups and bars of the old one ([`Engine::carry_whole`]).

use std::collections::{BTreeMap, BTreeSet};

use js_lease::Timestamp;
use js_provider::Candidate;
use js_provider::StartSpec;
use js_registry::{Exclusion, Reservation, Standing, WaitingWhole, WholeRound};
use js_sched::{Bar, Event, NodePhase, OpState, Request, RestoredMark, RestoredWhole, Scheduler};
use js_types::sched::{
    FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, Resources, WaiterId, WorkKey,
    WorkerId,
};
use js_types::{Admission, HostId, RequestId};

use super::{Engine, Placed, TERM};
use crate::map;

/// The term the leases of whole-node requests are numbered in, apart from the registry's
/// leases ([`TERM`]) and the jobs' ([`super::jobs::JOB_TERM`]).
pub const WHOLE_TERM: u64 = 3;

/// The give-ups and bars of a whole-node request, carried over a rebuild until the request
/// is queued again.
pub(super) type Carried = (u32, BTreeMap<WorkerId, Bar>);

/// The reason a tainted host is quarantined in the engine.
const TAINTED: &str = "tainted in the registry";

impl Engine {
    /// Runs one pass of the whole-node queue over `round` at `now` (see the module
    /// documentation) and answers the hosts set aside for the presented requests, as the
    /// registry shows them: a pending grant as an assigned reservation, a mark with its
    /// deadline and the end of its tail.
    pub fn whole_round(&mut self, round: WholeRound, now: FarmTime) -> Vec<Reservation> {
        // Nothing waits and nothing is queued: the engine takes nothing of the round, and
        // places as it does for a pool with no whole-node request.
        if round.waiting.is_empty() & self.whole.is_empty() {
            self.round = None;
            self.stand_down(now);
            return Vec::new();
        }
        self.apply_round(round, now);
        self.whole_tick(now);
        self.reservations(now)
    }

    /// The answer to `spec` when it is the start of a whole-node request the engine queues:
    /// the index in `open` of the host its pending grant is on, which the request now holds
    /// as the registry's lease (`Some(None)` while it has none there: it waits). `None` for
    /// any other start.
    pub fn commit_whole(
        &mut self,
        spec: &StartSpec,
        open: &[Candidate],
        now: FarmTime,
    ) -> Option<Option<usize>> {
        let whole = matches!(spec.class().admission(), Admission::Whole);
        let id = spec.request_id().filter(|_| whole)?;
        let operation = *self.whole.get(id)?;
        self.whole_tick(now);
        let Some((lease, worker)) = self.pending(operation) else {
            return Some(None);
        };
        let at = open.iter().position(|c| map::worker(&c.host) == worker);
        let committed = at.inspect(|index| {
            self.whole.remove(id);
            let placed = Placed { lease, operation };
            self.hold(&open[*index].host, spec.lease(), placed);
        });
        Some(committed)
    }

    /// Takes `round` (steps 1 to 5 of the module documentation) and keeps it for a rebuild.
    pub(super) fn apply_round(&mut self, round: WholeRound, now: FarmTime) {
        self.now = self.now.max(now);
        let settings = self.round.as_ref().map(|kept| &kept.settings);
        if settings != Some(&round.settings) {
            let sched = std::mem::replace(&mut self.sched, Scheduler::new(TERM));
            self.sched = sched.with_whole_node(round.settings.clone());
        }
        self.take_lives(&round.lives);
        let waiting: BTreeSet<&RequestId> = round.waiting.iter().map(|w| &w.id).collect();
        // A request that left the wait list is cancelled; one whose operation the
        // scheduler finished (refused as unservable) is cancelled and queued again.
        let left: Vec<(RequestId, OperationId)> = self
            .whole
            .iter()
            .filter(|(id, operation)| {
                let state = self.sched.state(**operation);
                !waiting.contains(id) | state.is_none_or(OpState::is_done)
            })
            .map(|(id, operation)| (id.clone(), *operation))
            .collect();
        for (id, operation) in left {
            self.whole.remove(&id);
            let _ = self.sched.cancel_whole(operation);
        }
        self.stand(&round.hosts, now);
        self.mark(&round.outside);
        for waiting in &round.waiting {
            let queued = self.whole.get(&waiting.id).copied();
            let operation = queued.unwrap_or_else(|| self.submit_whole(waiting, now));
            for (host, exclusion) in &waiting.exclusions {
                let bar = bar_of(*exclusion);
                let _ = self.sched.bar_whole(operation, map::worker(host), bar);
            }
        }
        self.round = Some(round);
    }

    /// Sets the lives of the leases the engine holds from `lives`, the registry's, by the
    /// work key of each lease's operation.
    fn take_lives(&mut self, lives: &BTreeMap<js_types::LeaseId, u32>) {
        let held = self.on.values().flat_map(|leases| leases.iter());
        let known = held.filter_map(|(lease, placed)| {
            let minutes = *lives.get(lease)?;
            let key = self.sched.request(placed.operation)?.key.clone();
            Some((key, minutes))
        });
        self.lives.set(known.collect());
    }

    /// Follows how the registry stands each of `hosts` (step 3 of the module
    /// documentation).
    fn stand(&mut self, hosts: &[js_registry::WholeHost], now: FarmTime) {
        let own = self.whole_hosts();
        let idle = |engine: &Self, host: &js_registry::WholeHost| {
            let worker = map::worker(&host.id);
            engine.sched.leases_on(&worker).is_empty() & !own.contains(&host.id)
        };
        // Whether some idle host the registry would book now is free for a request.
        let spare = hosts.iter().any(|host| host.open & idle(self, host));
        for host in hosts {
            let worker = map::worker(&host.id);
            // A host the registry would not book now that carries nothing (restoring after
            // a whole-node lease, a run still listed, cordoned by an operator) is granted to
            // no request while one it would book is free; one a request already holds stays
            // held, and with none free a request takes it, to wait there as on a mark.
            let shut = idle(self, host) & !host.open & spare;
            let cordon = (host.standing == Standing::Lost) | shut;
            let cordoned = self.cordoned.contains(&host.id);
            if cordon & !cordoned {
                self.cordoned.insert(host.id.clone());
                self.feed(
                    now,
                    Event::Cordon {
                        worker: worker.clone(),
                    },
                );
            }
            if !cordon & cordoned {
                self.cordoned.remove(&host.id);
                self.feed(
                    now,
                    Event::Uncordon {
                        worker: worker.clone(),
                    },
                );
            }
            // A lost host is not heard from: its phase is left as it is.
            let phase = match host.standing {
                Standing::Tainted => Some(NodePhase::Quarantined(TAINTED.to_owned())),
                Standing::Leaving => Some(NodePhase::Draining),
                Standing::Lost => None,
                Standing::Cordoned | Standing::Live => Some(NodePhase::Serving),
            };
            // A lost host keeps the phase the engine last gave it.
            match &phase {
                Some(NodePhase::Serving) => {
                    self.phased.remove(&host.id);
                }
                Some(_) => {
                    self.phased.insert(host.id.clone());
                }
                None => {}
            }
            if let Some(phase) = phase {
                self.feed(now, Event::Phase { worker, phase });
            }
        }
        let gone = [Standing::Lost, Standing::Tainted, Standing::Leaving];
        let off: BTreeSet<WorkerId> = hosts
            .iter()
            .filter(|host| gone.contains(&host.standing))
            .map(|host| map::worker(&host.id))
            .collect();
        self.unplace_whole_where(|worker| off.contains(worker));
    }

    /// Lets go of what the engine set on the hosts for the queue ([`Engine::stand`]) once the
    /// queue is empty: every host it cordoned is uncordoned and every host it gave another
    /// phase serves again, so the engine places as it does for a pool with no whole-node
    /// request.
    fn stand_down(&mut self, now: FarmTime) {
        for host in std::mem::take(&mut self.cordoned) {
            let worker = map::worker(&host);
            self.feed(now, Event::Uncordon { worker });
        }
        for host in std::mem::take(&mut self.phased) {
            let worker = map::worker(&host);
            let phase = NodePhase::Serving;
            self.feed(now, Event::Phase { worker, phase });
        }
    }

    /// Queues `waiting`, a request the engine does not queue yet, at the back of its queue
    /// (step 5 of the module documentation): its operation.
    fn submit_whole(&mut self, waiting: &WaitingWhole, now: FarmTime) -> OperationId {
        let operation = self.sched.next_operation();
        let request = Request {
            key: WorkKey::new("whole", waiting.id.to_string()),
            qos: Qos::Batch,
            kind: LeaseKind::WholeMachine,
            // Every host is booked as the pool's shape, and the registry queues only a
            // request the pool's shape fits: any worker fits it.
            resources: Resources::new(0, 0),
            needs: js_caps::Request::default(),
            hermetic: false,
            do_not_cache: true,
            whole_worker: false,
            pin: None,
        };
        let submit = Event::Submit {
            waiter: WaiterId(operation.0),
            request,
        };
        self.feed(now, submit);
        self.sched.set_max_wall(operation, waiting.max_wall);
        let (give_ups, bars) = self.carried.remove(&waiting.id).unwrap_or_default();
        // A pending grant is not a mark: a request that had one takes its quiet host again.
        let mark = waiting.mark.as_ref().filter(|mark| !mark.assigned);
        let mark = mark.map(|mark| RestoredMark {
            worker: map::worker(&mark.host),
            made: farm(mark.at),
            deadline: farm(mark.deadline),
            tail_end: mark.tail_end.map(FarmTime::from_millis),
        });
        let restored = |mark| RestoredWhole {
            operation,
            queued_at: farm(waiting.since),
            give_ups: give_ups.max(waiting.give_ups),
            bars: bars.clone(),
            mark,
        };
        // A mark the scheduler cannot hold (its host is not a worker, or another request
        // marks it) is let go: the request marks again.
        if self.sched.restore_whole(restored(mark)).is_err() {
            let _ = self.sched.restore_whole(restored(None));
        }
        self.whole.insert(waiting.id.clone(), operation);
        operation
    }

    /// One placement round in [`WHOLE_TERM`], for the whole-node requests the engine
    /// queues: none when it queues none. The newest pending grants beyond the whole-node
    /// ceiling are put back first. The term and the sequence are as they were after.
    pub(super) fn whole_tick(&mut self, now: FarmTime) {
        if self.whole.is_empty() {
            return;
        }
        self.drop_over_ceiling(now);
        let (operation, seq) = (self.sched.next_operation(), self.sched.next_seq());
        self.sched
            .recover(WHOLE_TERM, operation, self.whole_seq, Vec::new());
        self.feed(now, Event::Tick);
        self.whole_seq = self.sched.next_seq();
        self.sched.recover(TERM, operation, seq, Vec::new());
    }

    /// Puts back every whole-node request granted outside [`WHOLE_TERM`] (by a round that
    /// placed a start or a job, or by a worker uncordoned): the next whole round grants it
    /// again, numbered in its own term.
    pub(super) fn settle_whole(&mut self) {
        let stray: Vec<SchedLeaseId> = self
            .whole
            .values()
            .filter_map(|operation| self.pending(*operation))
            .filter(|(lease, _)| lease.term != WHOLE_TERM)
            .map(|(lease, _)| lease)
            .collect();
        for lease in stray {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// Puts back the pending grants on the workers `off` names: the requests are queued
    /// again, keeping their queue time.
    pub(super) fn unplace_whole_where(&mut self, off: impl Fn(&WorkerId) -> bool) {
        let on: Vec<SchedLeaseId> = self
            .whole
            .values()
            .filter_map(|operation| self.pending(*operation))
            .filter(|(_, worker)| off(worker))
            .map(|(lease, _)| lease)
            .collect();
        for lease in on {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// The lease and the worker of the grant `operation`, a queued whole-node request, has
    /// and the registry has not committed, if it has one.
    fn pending(&self, operation: OperationId) -> Option<(SchedLeaseId, WorkerId)> {
        match self.sched.state(operation) {
            Some(OpState::Leased { lease, worker, .. }) => Some((*lease, worker.clone())),
            _ => None,
        }
    }

    /// The pending grants on `worker`: a heartbeat of its host lists them as running, so
    /// none is given up for a start it has not been sent.
    pub(super) fn pending_on(&self, worker: &WorkerId) -> Vec<SchedLeaseId> {
        let pending = self.whole.values().filter_map(|op| self.pending(*op));
        let here = pending.filter(|(_, on)| on == worker);
        here.map(|(lease, _)| lease).collect()
    }

    /// How many holds each whole-node request the engine queues has given up at their
    /// deadline, those that gave one up ([`Placer::whole_give_ups`]).
    ///
    /// [`Placer::whole_give_ups`]: js_registry::Placer::whole_give_ups
    pub(crate) fn whole_give_ups(&self) -> BTreeMap<RequestId, u32> {
        let counted = self.whole.iter().map(|(id, operation)| {
            let count = self.sched.whole_give_ups(*operation);
            (id.clone(), count)
        });
        counted.filter(|(_, count)| *count > 0).collect()
    }

    /// Puts back the newest pending grants beyond the whole-node ceiling, with the marks
    /// (their requests are queued again, keeping their queue time, with no give-up and no
    /// bar), as the registry drops its newest reservations, an assigned one included, when
    /// the ceiling falls; a committed lease is never touched. The scheduler counts a
    /// pending grant as a held whole worker, which its own drop at the round's start never
    /// touches, so the engine drops it here, before the round. A mark among the newest is
    /// left to that drop, which then finds the same marks beyond the ceiling.
    fn drop_over_ceiling(&mut self, now: FarmTime) {
        let summary = self.sched.whole_summary();
        let excess = summary.map_or(0, |summary| summary.whole.saturating_sub(summary.ceiling));
        if excess == 0 {
            return;
        }
        let mut held = self.reservations(now);
        // Newest first, as `js_whole::caps::ceiling_excess` takes them.
        held.sort_by(|a, b| (b.at, &b.request).cmp(&(a.at, &a.request)));
        let newest = held.into_iter().take(excess).filter(|held| held.assigned);
        let leases: Vec<SchedLeaseId> = newest
            .filter_map(|held| self.whole.get(&held.request))
            .filter_map(|operation| self.pending(*operation))
            .map(|(lease, _)| lease)
            .collect();
        for lease in leases {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// The hosts the engine's whole-node requests hold: marked, or granted and pending.
    pub(super) fn whole_hosts(&self) -> BTreeSet<HostId> {
        let held = self.whole.values().filter_map(|operation| {
            let marked = self.sched.whole_mark(*operation).map(|mark| &mark.worker);
            let pending = self.pending(*operation).map(|(_, worker)| worker);
            let worker = pending.or_else(|| marked.cloned())?;
            map::host_id(&worker).ok()
        });
        held.collect()
    }

    /// The reservations of the requests the engine queues, as the registry shows them.
    fn reservations(&self, now: FarmTime) -> Vec<Reservation> {
        let waiting = self.round.iter().flat_map(|round| round.waiting.iter());
        let earlier: BTreeMap<&RequestId, &Reservation> = waiting
            .filter_map(|waiting| Some((&waiting.id, waiting.mark.as_ref()?)))
            .collect();
        let held = self.whole.iter().filter_map(|(id, operation)| {
            if let Some((_, worker)) = self.pending(*operation) {
                // Set aside since the mark it had, if it had one: it keeps its age.
                let at = earlier.get(id).map_or(stamp(now), |mark| mark.at);
                return Some(Reservation {
                    request: id.clone(),
                    host: map::host_id(&worker).ok()?,
                    at,
                    deadline: at,
                    assigned: true,
                    tail_end: None,
                });
            }
            let mark = self.sched.whole_mark(*operation)?;
            Some(Reservation {
                request: id.clone(),
                host: map::host_id(&mark.worker).ok()?,
                at: stamp(mark.made),
                deadline: stamp(mark.deadline),
                assigned: false,
                tail_end: mark.tail_end.map(FarmTime::as_millis),
            })
        });
        held.collect()
    }

    /// Takes the whole-node queue of `old`, the engine this one is built to replace: the
    /// numbering of its leases, the give-ups and bars of its requests, and its last round,
    /// with the marks it holds now, which queues the requests here again.
    pub(super) fn carry_whole(&mut self, old: &Self, now: FarmTime) {
        self.whole_seq = old.whole_seq;
        self.carried = old.carried.clone();
        for (id, operation) in &old.whole {
            let give_ups = old.sched.whole_give_ups(*operation);
            let bars = old.sched.whole_barred(*operation, now);
            self.carried.insert(id.clone(), (give_ups, bars));
        }
        let Some(mut round) = old.round.clone() else {
            return;
        };
        let held = old.reservations(now);
        for waiting in &mut round.waiting {
            let mark = held.iter().find(|mark| mark.request == waiting.id);
            waiting.mark = mark.cloned();
        }
        self.apply_round(round, now);
    }
}

/// The hard bar of `exclusion`: the host is neither marked for the request nor granted to
/// it while it holds.
fn bar_of(exclusion: Exclusion) -> Bar {
    match exclusion {
        Exclusion::Until(end) => Bar::Until(farm(end)),
        Exclusion::ForLife => Bar::ForLife,
    }
}

/// `at` on the registry's clock as farm time: the same milliseconds.
fn farm(at: Timestamp) -> FarmTime {
    FarmTime::from_millis(at.millis())
}

/// `at` as a time on the registry's clock.
fn stamp(at: FarmTime) -> Timestamp {
    Timestamp::from_millis(at.as_millis())
}

#[cfg(test)]
#[path = "whole_tests.rs"]
mod tests;
