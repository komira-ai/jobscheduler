//! The drain cost of a worker, for the worker a blocked whole-machine request holds, and
//! the wait before it holds one, the deadline of the hold and its give-up
//! ([`Scheduler::with_whole_node`]); and the caps that keep whole-machine requests from
//! taking a pool (`max_whole_node`, `max_draining`, `keep_for_ordinary`, the last
//! capable worker of an older ordinary request), with the ceiling that falls when
//! workers do.
//!
//! The pool the caps count is every registered worker that serves whole-machine leases:
//! the scheduler has no pools of its own, so one ceiling covers them all.
//! Workers of every platform count toward that one ceiling and one `max_draining`.
//! `max_whole_node_per_consumer` is not enforced: no request, waiter or qos here names
//! a consumer, so there is nothing to count; the lease server or the registry, where the
//! caller's identity exists, enforces it ([`Scheduler::with_whole_node`]).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_types::sched::{FarmTime, LeaseId, LeaseKind, NodePhase, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::{
    Blocked, Capability, Counts, Limits, Mark, OrdinaryRequest, capable_hosts, ceiling,
    ceiling_excess, last_capable_blocks, may_reserve,
};
use js_whole::cost::{DrainCost, HostLoad, drain_cost, reserve_deadline, tail};

use super::{Scheduler, Worker};
use crate::servable::{Placed, Servable, serves};

/// Why a whole-machine request that fits nowhere holds no worker though it has waited
/// out `drain_after` ([`Scheduler::whole_wait`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WholeWait {
    /// A cap of the pool stops a new mark.
    Cap(Blocked),
    /// Every worker it could hold is the last one an older ordinary request could run on,
    /// and `allow_last_capable` is off.
    LastCapable,
}

/// The mark a queued whole-machine request has on the worker it holds while that worker
/// drains ([`Scheduler::whole_mark`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeMark {
    /// The held worker.
    pub worker: WorkerId,
    /// When the mark was made.
    pub made: FarmTime,
    /// When the hold is given up if the worker is still busy.
    pub deadline: FarmTime,
}

/// The drain state of the whole-node requests: marks, give-up counts and back-offs, kept
/// in memory only. Empty unless whole-node settings are on.
#[derive(Clone, Debug, Default)]
pub(super) struct Drain {
    /// The mark of each request that holds a worker, as of the last placement round.
    pub(super) marks: BTreeMap<OperationId, WholeMark>,
    /// How many holds each request has given up.
    give_ups: BTreeMap<OperationId, u32>,
    /// The workers each request avoids, and until when (exclusive).
    backoff: BTreeMap<OperationId, BTreeMap<WorkerId, FarmTime>>,
    /// Why each request without a mark waits, as of the last placement round.
    pub(super) waits: BTreeMap<OperationId, WholeWait>,
}

/// What a request may do in one placement round, from [`Drain::turn`].
pub(super) struct Turn {
    /// The mark the request keeps, if its hold is still on.
    pub(super) kept: Option<WholeMark>,
    /// The workers the request may not hold now.
    pub(super) barred: BTreeSet<WorkerId>,
}

impl Drain {
    /// The workers `operation` avoids at `now`, each with when its back-off ends
    /// (exclusive): those whose back-off has not ended.
    pub(super) fn barred_until(
        &self,
        operation: OperationId,
        now: FarmTime,
    ) -> BTreeMap<WorkerId, FarmTime> {
        self.backoff
            .get(&operation)
            .map_or_else(BTreeMap::new, |m| {
                m.iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(worker, until)| (worker.clone(), *until))
                    .collect()
            })
    }

    /// The workers `operation` avoids at `now`: those whose back-off has not ended.
    pub(super) fn barred(&self, operation: OperationId, now: FarmTime) -> BTreeSet<WorkerId> {
        self.barred_until(operation, now).into_keys().collect()
    }

    /// Whether the hold of `operation` is given up when its deadline comes: not once it
    /// has given up `max_reserve_attempts` holds.
    pub(super) fn gives_up(&self, operation: OperationId, cfg: &WholeNodeConfig) -> bool {
        self.give_ups.get(&operation).copied().unwrap_or(0) < cfg.max_reserve_attempts
    }

    /// `avoid` with the workers `barred` added.
    pub(super) fn avoiding<'a>(
        avoid: &'a BTreeSet<WorkerId>,
        barred: &BTreeSet<WorkerId>,
    ) -> Cow<'a, BTreeSet<WorkerId>> {
        if barred.is_empty() {
            Cow::Borrowed(avoid)
        } else {
            Cow::Owned(avoid.union(barred).cloned().collect())
        }
    }

    /// Whether `operation`, queued since `queued_at`, may hold a worker this round, and
    /// what it keeps. `None` before `drain_after` has passed. A hold that has reached its
    /// deadline and is still not placed (a worker that emptied would have been taken
    /// before this) is given up here: counted, dropped, and its worker barred until the
    /// back-off ends. A request that has given up `max_reserve_attempts` holds is not
    /// given up again.
    pub(super) fn turn(
        &mut self,
        operation: OperationId,
        queued_at: FarmTime,
        now: FarmTime,
        cfg: &WholeNodeConfig,
    ) -> Option<Turn> {
        if now.as_millis() < queued_at.as_millis().saturating_add(cfg.drain_after_ms) {
            return None;
        }
        let mut kept = None;
        if let Some(mark) = self.marks.remove(&operation) {
            // Once the count has reached the cap the hold is never given up: its deadline
            // is the request's own wall time, and a worker may hold a lease with a longer
            // one, so reaching it must not end the hold (whole-node.md section 4).
            if now >= mark.deadline && self.gives_up(operation, cfg) {
                *self.give_ups.entry(operation).or_default() += 1;
                let until =
                    FarmTime::from_millis(now.as_millis().saturating_add(cfg.reserve_backoff_ms));
                self.backoff
                    .entry(operation)
                    .or_default()
                    .insert(mark.worker, until);
            } else {
                kept = Some(mark);
            }
        }
        Some(Turn {
            kept,
            barred: self.barred(operation, now),
        })
    }

    /// The mark `operation` has on `worker` at `now`: the one it kept, when it is on the
    /// same worker, else a new one on `worker` whose deadline follows [`reserve_deadline`]
    /// and whose time of making is the kept mark's: a hold that moves keeps its age.
    pub(super) fn mark(
        &self,
        operation: OperationId,
        worker: &WorkerId,
        kept: Option<WholeMark>,
        now: FarmTime,
        cfg: &WholeNodeConfig,
        max_wall: Option<Duration>,
    ) -> WholeMark {
        let made = kept.as_ref().map_or(now, |m| m.made);
        if let Some(kept) = kept.filter(|m| &m.worker == worker) {
            return kept;
        }
        let wall = max_wall.map_or(u64::MAX, |d| {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        });
        let attempts = self.give_ups.get(&operation).copied().unwrap_or(0);
        let wait = reserve_deadline(attempts, cfg, wall);
        WholeMark {
            worker: worker.clone(),
            made,
            deadline: FarmTime::from_millis(now.as_millis().saturating_add(wait)),
        }
    }

    /// Forgets `operation`.
    pub(super) fn forget(&mut self, operation: OperationId) {
        self.marks.remove(&operation);
        self.give_ups.remove(&operation);
        self.backoff.remove(&operation);
        self.waits.remove(&operation);
    }
}

/// What one placement round knows for the caps: the limits, the whole leases it counts,
/// the ordinary requests ahead that wait, and the marks and waits it makes.
pub(super) struct Round {
    /// Whether whole-node settings are on.
    on: bool,
    /// The limits of the caps.
    limits: Limits,
    /// Workers held by a whole-machine lease at the start of the round, and the
    /// whole-machine requests placed since.
    held: usize,
    /// The queued ordinary requests not placed so far, with their queue positions.
    ordinary: Vec<(u64, OperationId)>,
    /// The marks of this round, new and kept.
    pub(super) marks: BTreeMap<OperationId, WholeMark>,
    /// The waits of this round.
    pub(super) waits: BTreeMap<OperationId, WholeWait>,
}

impl Round {
    /// Counts a whole-machine request placed this round as a whole worker.
    pub(super) fn placed_whole(&mut self) {
        self.held += 1;
    }

    /// Notes an ordinary request that fits nowhere and waits at queue `position`.
    pub(super) fn waiting_ordinary(&mut self, position: u64, operation: OperationId) {
        if self.on {
            self.ordinary.push((position, operation));
        }
    }
}

/// The ceiling of `limits`: the most workers that may be whole at once.
pub(super) fn ceiling_of(limits: Limits) -> usize {
    ceiling(
        limits.max_whole_node,
        limits.capable_hosts,
        limits.keep_for_ordinary,
    )
}

/// How a worker counts toward the capable hosts of the pool.
fn capability(worker: &Worker, now: FarmTime) -> Capability {
    if !worker.alive(now) {
        Capability::Lost
    } else {
        match worker.phase {
            // Needs an operator, as a tainted host does.
            NodePhase::Quarantined(_) => Capability::Tainted,
            // Leaving the pool.
            NodePhase::Draining => Capability::Releasing,
            _ => Capability::Capable,
        }
    }
}

impl Scheduler {
    /// The mark queued whole-machine `operation` has on the worker it holds, with
    /// whole-node settings on ([`Scheduler::with_whole_node`]): its time and deadline. The
    /// marks, the give-up counts and the back-offs are in memory only: a recovered
    /// scheduler starts without them, so a request that gave up holds again from a count
    /// of zero.
    #[must_use]
    pub fn whole_mark(&self, operation: OperationId) -> Option<&WholeMark> {
        self.drain.marks.get(&operation)
    }

    /// How many holds `operation` has given up at their deadline, with whole-node
    /// settings on; zero after a recovery.
    #[must_use]
    pub fn whole_give_ups(&self, operation: OperationId) -> u32 {
        self.drain.give_ups.get(&operation).copied().unwrap_or(0)
    }

    /// Why queued whole-machine `operation` holds no worker although it has waited out
    /// `drain_after`, with whole-node settings on: the cap that stops it, or that every
    /// worker it could hold is the last one of an older ordinary request. `None` when it
    /// holds a worker, is placed, or has not yet reached its turn; as of the last
    /// placement round.
    #[must_use]
    pub fn whole_wait(&self, operation: OperationId) -> Option<WholeWait> {
        self.drain.waits.get(&operation).copied()
    }

    /// Names the longest `operation` may run once leased, which the caller knows from the
    /// request's wall time: no scheduler input and no log record carries it. A lease's end
    /// bound is its start plus this; a lease whose operation has none is taken to end at
    /// the latest time there is, so a worker holding it drains last. Set it with the
    /// submit and again for an operation taken up after a restart (a recovered lease's
    /// start is the recovery, so its bound is that plus this). Dropped with the operation.
    pub fn set_max_wall(&mut self, operation: OperationId, max_wall: Duration) {
        self.max_walls.insert(operation, max_wall);
    }

    /// The registered workers by drain cost, cheapest first: by the least tail, whether or
    /// not whole-node settings are on. `placed` are this round's placements, not yet held.
    pub(super) fn drain_order(&self, placed: &[(OperationId, Placed)]) -> Vec<DrainCost> {
        // Every held lease belongs to an operation that holds it. Each entry is the end
        // bound of a lease and its estimated end.
        let mut ends: BTreeMap<&WorkerId, Vec<(u64, Option<u64>)>> = BTreeMap::new();
        for held in self.held.values() {
            let start = held.started_at.unwrap_or(held.granted_at).as_millis();
            let end = self.end_times(held.operation, start);
            let holding = self.ops[&held.operation].state.holding();
            holding
                .into_iter()
                .for_each(|(_, worker)| ends.entry(worker).or_default().push(end));
        }
        for (operation, placed) in placed {
            let end = self.end_times(*operation, self.now.as_millis());
            ends.entry(&placed.worker).or_default().push(end);
        }
        let loads: Vec<HostLoad> = self
            .workers
            .iter()
            .map(|(name, w)| {
                let (end_bounds, est_ends) =
                    ends.remove(name).unwrap_or_default().into_iter().unzip();
                HostLoad {
                    id: name.as_str().to_owned(),
                    leases: w.leases,
                    end_bounds,
                    est_ends,
                }
            })
            .collect();
        drain_cost(&loads, self.now.as_millis())
    }

    /// The end bound (start plus max wall time, the latest time there is for a lease with
    /// none) and the estimated end (start plus the P90 of the wall minutes of its key, raw;
    /// unknown when the key has none) of a lease of `operation` started at `start`.
    fn end_times(&self, operation: OperationId, start: u64) -> (u64, Option<u64>) {
        let bound = self.max_walls.get(&operation).map_or(u64::MAX, |wall| {
            start.saturating_add(u64::try_from(wall.as_millis()).unwrap_or(u64::MAX))
        });
        let key = &self.ops[&operation].request.key;
        let estimate = self
            .admission
            .wall_minutes_p90(key)
            .map(|minutes| start.saturating_add(u64::from(minutes).saturating_mul(60_000)));
        (bound, estimate)
    }

    /// The end of each lease held or placed this round, as farm milliseconds: its estimate
    /// cut to its bound, else its bound; `None` for a lease with neither. A lease placed
    /// this round starts now.
    pub(super) fn lease_ends(
        &self,
        placed: &[(OperationId, Placed)],
    ) -> BTreeMap<LeaseId, Option<u64>> {
        let end = |(bound, estimate): (u64, Option<u64>)| {
            tail(&HostLoad {
                id: String::new(),
                leases: 1,
                end_bounds: vec![bound],
                est_ends: vec![estimate],
            })
        };
        let held = self.held.iter().map(|(lease, held)| {
            let start = held.started_at.unwrap_or(held.granted_at).as_millis();
            (*lease, end(self.end_times(held.operation, start)))
        });
        let now = self.now.as_millis();
        let this_round = (0u64..).zip(placed).map(|(i, (operation, _))| {
            let lease = LeaseId::new(self.term, self.next_seq + i);
            (lease, end(self.end_times(*operation, now)))
        });
        held.chain(this_round).collect()
    }
}

impl Scheduler {
    /// The workers a whole-machine lease holds, with the time it was granted.
    fn whole_held(&self) -> BTreeMap<&WorkerId, FarmTime> {
        self.held
            .values()
            .filter(|h| self.ops[&h.operation].request.kind == LeaseKind::WholeMachine)
            .filter_map(|h| {
                let (_, worker) = self.ops[&h.operation].state.holding()?;
                Some((worker, h.granted_at))
            })
            .collect()
    }

    /// The limits of the caps now, from the settings and the workers.
    fn whole_limits(&self, cfg: &WholeNodeConfig) -> Limits {
        let hosts: Vec<Capability> = self
            .workers
            .values()
            .filter(|w| serves(&w.caps, LeaseKind::WholeMachine))
            .map(|w| capability(w, self.now))
            .collect();
        Limits {
            max_whole_node: cfg.max_whole_node,
            capable_hosts: capable_hosts(&hosts),
            keep_for_ordinary: cfg.keep_for_ordinary,
            max_draining: cfg.max_draining,
            per_consumer: None,
        }
    }

    /// The start of a placement round: if the ceiling has fallen below the whole workers
    /// (a worker taken out of the pool, a cap lowered), drops the newest marks beyond it,
    /// never a held lease. A dropped mark counts no give-up and bars no worker. The
    /// round's state for the caps.
    pub(super) fn whole_round(&mut self) -> Round {
        let Some(cfg) = self.whole_node.clone() else {
            return Round {
                on: false,
                // Unread: no caps are checked without settings.
                limits: Limits {
                    max_whole_node: 0,
                    capable_hosts: 0,
                    keep_for_ordinary: 0,
                    max_draining: 0,
                    per_consumer: None,
                },
                held: 0,
                ordinary: Vec::new(),
                marks: BTreeMap::new(),
                waits: BTreeMap::new(),
            };
        };
        let limits = self.whole_limits(&cfg);
        let mut marks: Vec<Mark> = self
            .whole_held()
            .iter()
            .map(|(worker, at)| Mark::Held {
                host: worker.as_str().to_owned(),
                at: at.as_millis(),
            })
            .collect();
        let held = marks.len();
        // Zero-padded, so that the order of the names is the order of the ids.
        let names: BTreeMap<String, OperationId> = self
            .drain
            .marks
            .keys()
            .map(|id| (format!("{:020}", id.0), *id))
            .collect();
        marks.extend(self.drain.marks.iter().map(|(id, m)| Mark::Reserved {
            request: format!("{:020}", id.0),
            at: m.made.as_millis(),
        }));
        let bound = ceiling_of(limits);
        for name in ceiling_excess(&marks, bound) {
            self.drain.marks.remove(&names[&name]);
        }
        Round {
            on: true,
            limits,
            held,
            ordinary: Vec::new(),
            marks: BTreeMap::new(),
            waits: BTreeMap::new(),
        }
    }

    /// The worker whole-machine `id`, at queue `position`, holds this round and its mark,
    /// when it holds one: `kept` stays on its worker without a check (that mark is
    /// already counted); a new mark is made only if the caps allow it ([`may_reserve`])
    /// and the worker is not the last capable one of an older ordinary request, in which
    /// case the next worker by drain cost is tried. `Err` is the reason it holds none.
    /// `Ok(None)` is no worker that could.
    ///
    /// Head holds are not marks: `reserved` keeps a request off a worker a head holds, and
    /// no cap counts it.
    pub(super) fn hold_whole(
        &self,
        servable: &mut Servable,
        round: &Round,
        (id, position): (OperationId, u64),
        reserved: &BTreeSet<WorkerId>,
        placed: &[(OperationId, Placed)],
        turn: Turn,
    ) -> Result<Option<(WorkerId, WholeMark)>, WholeWait> {
        let cfg = self
            .whole_node
            .as_ref()
            .expect("whole-node settings are on");
        let op = &self.ops[&id];
        let mut out: BTreeSet<WorkerId> = reserved.union(&turn.barred).cloned().collect();
        let avoid = Drain::avoiding(&op.avoid, &turn.barred);
        let kept = turn.kept.as_ref().map(|m| &m.worker);
        let drain = self.drain_order(placed);
        let max_wall = self.max_walls.get(&id).copied();
        let mut wait = None;
        loop {
            // The pick is made again every round, with no stickiness; `kept` is only the
            // worker the mark was on, whose cap check is already made.
            let Some(name) = servable.reserve(&self.workers, &op.request, &out, &avoid, &drain)
            else {
                return wait.map_or(Ok(None), Err);
            };
            if kept != Some(&name) {
                may_reserve(self.whole_counts(round), round.limits).map_err(WholeWait::Cap)?;
                if self.last_capable(servable, round, &name, position, cfg) {
                    wait = Some(WholeWait::LastCapable);
                    out.insert(name);
                    continue;
                }
            }
            let mark = self
                .drain
                .mark(id, &name, turn.kept, self.now, cfg, max_wall);
            return Ok(Some((name, mark)));
        }
    }

    /// Whether whole-machine `id` may take a worker that has no lease at once this round.
    /// The take adds one whole worker and drains none: it is checked against the ceiling
    /// and the floor kept for ordinary work ([`may_reserve`] with no draining worker
    /// counted), and `max_draining` does not apply. A request that already holds a mark
    /// is not checked: its mark is counted, and the take only turns it into a lease.
    /// The last-capable rule is not read here: an older ordinary request that a quiet
    /// worker could run is placed on it before this request is reached. Narrow caveat:
    /// an older ordinary request that is capable by facts but blocked by admission, or
    /// left over by the round's placement limit, is not placed first, so a quiet take
    /// can take its last capable worker.
    pub(super) fn quiet_take(&self, round: &Round, id: OperationId) -> bool {
        self.whole_node.is_none()
            || self.drain.marks.contains_key(&id)
            || may_reserve(
                Counts {
                    reserved: 0,
                    ..self.whole_counts(round)
                },
                round.limits,
            )
            .is_ok()
    }

    /// The limits and the counts the caps read between placement rounds: the same
    /// [`Scheduler::whole_limits`] and [`Scheduler::whole_counts`] a round reads, with
    /// the marks as the last round left them and none made yet.
    pub(super) fn whole_at_rest(&self, cfg: &WholeNodeConfig) -> (Limits, Counts, usize) {
        let held = self.whole_held().len();
        let round = Round {
            on: true,
            limits: self.whole_limits(cfg),
            held,
            ordinary: Vec::new(),
            marks: BTreeMap::new(),
            waits: BTreeMap::new(),
        };
        (round.limits, self.whole_counts(&round), held)
    }

    /// What the caps count now: whole workers (held leases, marks not yet reached this
    /// round, marks made or kept), and those marks that are on a busy worker.
    fn whole_counts(&self, round: &Round) -> Counts {
        let marks = || self.drain.marks.values().chain(round.marks.values());
        Counts {
            whole: round.held + marks().count(),
            reserved: marks()
                .filter(|m| self.workers.get(&m.worker).is_some_and(|w| w.leases > 0))
                .count(),
            consumer_whole: 0,
        }
    }

    /// Whether a mark on `host` for the request at `position` would take the last
    /// capable worker no whole mark holds of an older ordinary request that waits.
    fn last_capable(
        &self,
        servable: &mut Servable,
        round: &Round,
        host: &WorkerId,
        position: u64,
        cfg: &WholeNodeConfig,
    ) -> bool {
        if cfg.allow_last_capable {
            return false;
        }
        // Only a whole mark takes a worker from an ordinary request: a head's hold is its
        // own, and a worker it holds is one it can still run on.
        let marked: BTreeSet<WorkerId> = self
            .drain
            .marks
            .values()
            .chain(round.marks.values())
            .map(|m| m.worker.clone())
            .collect();
        let ordinary: Vec<OrdinaryRequest> = round
            .ordinary
            .iter()
            .map(|(position, id)| OrdinaryRequest {
                position: *position,
                unreserved_capable: servable
                    .capable(&self.workers, &self.ops[id].request, &marked)
                    .into_iter()
                    .map(|name| name.as_str().to_owned())
                    .collect(),
            })
            .collect();
        last_capable_blocks(host.as_str(), position, &ordinary, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: `forget` leaving one of the three maps behind: each holds an entry for the
    /// operation (a mark, a give-up count and a back-off), and none does after it.
    #[test]
    fn forget_drops_the_mark_the_count_and_the_backoff() {
        let cfg = WholeNodeConfig::dedicated(4);
        let op = OperationId(1);
        let other = OperationId(2);
        let worker = WorkerId::new("a");
        let mark = |deadline| WholeMark {
            worker: worker.clone(),
            made: FarmTime::from_millis(0),
            deadline: FarmTime::from_millis(deadline),
        };
        let mut drain = Drain::default();
        drain.marks.insert(op, mark(10));
        drain.marks.insert(other, mark(10));
        drain.turn(
            op,
            FarmTime::from_millis(0),
            FarmTime::from_millis(10),
            &cfg,
        );
        drain.marks.insert(op, mark(10));
        assert_eq!(drain.give_ups.get(&op), Some(&1));
        assert!(drain.backoff.contains_key(&op));
        drain.forget(op);
        assert!(!drain.marks.contains_key(&op));
        assert!(!drain.give_ups.contains_key(&op));
        assert!(!drain.backoff.contains_key(&op));
        assert!(drain.marks.contains_key(&other));
    }
}
