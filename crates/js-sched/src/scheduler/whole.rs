//! The drain cost of a worker, for the worker a blocked whole-machine request holds, and
//! the wait before it holds one and the deadline of the hold
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
//!
//! A mark belongs to its request: it stays on the worker it was made on until the request
//! is placed or is cancelled, whatever its deadline, and is never traded for a
//! worker that has become cheaper to drain, nor taken by another request's hold or mark
//! ([`Scheduler::hold_whole`]). It moves only when its worker can no longer hold the
//! request (gone, cordoned, barred to it). A worker a whole-machine lease holds is never
//! marked: no request waits behind another one's whole-machine lease.
//!
//! The whole-node state of each operation is kept in memory only: its queue time, its
//! mark (the worker, when it was made, its deadline and the end of the tail the worker
//! was chosen on), and the workers barred to it, each with a [`Bar`] (a back-off or a
//! hard bar set with [`Scheduler::bar_whole`]). The waits ([`Scheduler::whole_wait`])
//! are only the last round's reading. A caller that keeps the rest outside the scheduler
//! (a ledger, a store) puts it back with [`Scheduler::restore_whole`] after a restart,
//! and reads it with [`Scheduler::whole_mark`] and [`Scheduler::whole_barred`].
//!
//! [`Scheduler::restore_whole`], [`Scheduler::bar_whole`] and [`Scheduler::cancel_whole`] are
//! methods of the scheduler, not [`crate::Event`]s, as [`Scheduler::unplace`] is: the
//! caller learns what they say from outside the event stream (its ledger, a store, a
//! prepare that failed on the host) and calls them under the lock that serialises its
//! inputs. A replay of the events alone therefore does not rebuild what they set; the
//! replay property of the events is unchanged.

mod bar;
mod restore;
mod room;

pub use bar::Bar;
pub use restore::{RestoredMark, RestoredWhole, WholeError};
pub use room::Room;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_types::sched::{FarmTime, LeaseId, LeaseKind, NodePhase, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::{
    Blocked, Capability, Counts, Limits, Mark, OrdinaryRequest, capable_hosts, ceiling,
    ceiling_excess, last_capable_blocks, may_reserve,
};
use js_whole::cost::{DrainCost, HostLoad, drain_cost, tail};

use super::{Scheduler, Worker};
use crate::backfill;
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
    /// When the worker is expected to be drained at the latest: `drain_deadline`, else the
    /// request's wall limit, after the mark was made. Reaching it changes nothing: the
    /// mark stays on its worker, and nothing running there is stopped.
    pub deadline: FarmTime,
    /// The end of the worker's tail when the mark was made on it: when the request is
    /// expected to start there (`None`: not known). Taken once, when the mark is made,
    /// and kept while the mark stays on that worker; work after the request is placed
    /// there only as backfill that ends by it, or by the tail now if that ends sooner
    /// ([`Scheduler::with_backfill`]). It stays the bound when the tail now is not known,
    /// as a hold made again on the same worker keeps its earlier start; a mark made with
    /// none admits no backfill until it moves.
    pub tail_end: Option<FarmTime>,
}

/// The drain state of the whole-node requests: marks and bars, kept in memory only. Empty
/// unless whole-node settings are on.
#[derive(Clone, Debug, Default)]
pub(super) struct Drain {
    /// The mark of each request that holds a worker, as of the last placement round.
    pub(super) marks: BTreeMap<OperationId, WholeMark>,
    /// The workers each request is barred from, and how ([`Bar`]).
    bars: BTreeMap<OperationId, BTreeMap<WorkerId, Bar>>,
    /// Why each request without a mark waits, as of the last placement round.
    pub(super) waits: BTreeMap<OperationId, WholeWait>,
}

/// What a request may do in one placement round, from [`Drain::turn`].
pub(super) struct Turn {
    /// The mark the request had at the start of the round, if any.
    pub(super) kept: Option<WholeMark>,
    /// The workers the request may not hold now.
    pub(super) barred: BTreeSet<WorkerId>,
}

impl Drain {
    /// The bars of `operation` that hold at `now`.
    pub(super) fn bars_at(&self, operation: OperationId, now: FarmTime) -> BTreeMap<WorkerId, Bar> {
        self.bars
            .get(&operation)
            .map_or_else(BTreeMap::new, |bars| {
                bars.iter()
                    .filter(|(_, bar)| bar.holds_at(now))
                    .map(|(worker, bar)| (worker.clone(), *bar))
                    .collect()
            })
    }

    /// The workers `operation` is barred from at `now`, each with when its bar ends
    /// (exclusive; the end of time for a bar for life).
    pub(super) fn barred_until(
        &self,
        operation: OperationId,
        now: FarmTime,
    ) -> BTreeMap<WorkerId, FarmTime> {
        let bars = self.bars_at(operation, now).into_iter();
        bars.map(|(worker, bar)| (worker, bar.end())).collect()
    }

    /// The workers `operation` may not mark at `now`: every one barred to it.
    pub(super) fn barred(&self, operation: OperationId, now: FarmTime) -> BTreeSet<WorkerId> {
        self.bars_at(operation, now).into_keys().collect()
    }

    /// The workers `operation` is barred from at `now`, split as placement reads them:
    /// those hard-barred, which it is never granted, and those in a back-off, which it
    /// avoids as it avoids the workers of [`crate::Fate::avoid`].
    pub(super) fn kept_off(
        &self,
        operation: OperationId,
        now: FarmTime,
    ) -> (BTreeSet<WorkerId>, BTreeSet<WorkerId>) {
        let (hard, soft): (Vec<_>, Vec<_>) = self
            .bars_at(operation, now)
            .into_iter()
            .partition(|(_, bar)| bar.is_hard());
        let names = |bars: Vec<(WorkerId, Bar)>| bars.into_iter().map(|(w, _)| w).collect();
        (names(hard), names(soft))
    }

    /// The operations `worker` is barred to at `now`.
    pub(super) fn barred_operations<'a>(
        &'a self,
        worker: &'a WorkerId,
        now: FarmTime,
    ) -> impl Iterator<Item = OperationId> + 'a {
        self.bars
            .iter()
            .filter(move |(_, bars)| bars.get(worker).is_some_and(|bar| bar.holds_at(now)))
            .map(|(id, _)| *id)
    }

    /// Bars `worker` to `operation` with `bar` at `now`; the worker keeps the stronger bar.
    pub(super) fn bar(
        &mut self,
        operation: OperationId,
        worker: WorkerId,
        bar: Bar,
        now: FarmTime,
    ) {
        bar::set_bar(self.bars.entry(operation).or_default(), worker, bar, now);
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
    /// the mark it had. `None` before `drain_after` has passed. A hold is never given up:
    /// a mark past its deadline is kept as any other is, so a drain never ends while the
    /// worker's work runs (whole-node.md section 4).
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
        Some(Turn {
            kept: self.marks.remove(&operation),
            barred: self.barred(operation, now),
        })
    }

    /// The mark on `worker` at `now`: the one kept, when it is on the same worker, else a
    /// new one on `worker` whose deadline is `drain_deadline`, or the request's wall
    /// limit `max_wall` when none is set ([`WholeNodeConfig::drain_deadline_ms`]), after
    /// now, whose tail end is `tail` (the end of the worker's tail now) and whose time of
    /// making is the kept mark's: a mark moved off a worker that can no longer hold its
    /// request keeps its age.
    pub(super) fn mark(
        (worker, tail): (&WorkerId, Option<u64>),
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
        let wait = cfg.drain_deadline_ms(wall);
        WholeMark {
            worker: worker.clone(),
            made,
            deadline: FarmTime::from_millis(now.as_millis().saturating_add(wait)),
            tail_end: tail.map(FarmTime::from_millis),
        }
    }

    /// Puts back the bars of `operation`, replacing its own.
    fn restore(&mut self, operation: OperationId, bars: BTreeMap<WorkerId, Bar>) {
        self.bars.insert(operation, bars);
    }

    /// Forgets `operation`.
    pub(super) fn forget(&mut self, operation: OperationId) {
        self.marks.remove(&operation);
        self.bars.remove(&operation);
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
    /// whole-node settings on ([`Scheduler::with_whole_node`]): its time, its deadline and
    /// the tail end it was made with. The marks and the bars are in memory only: a
    /// recovered scheduler starts without them unless the caller puts them back
    /// ([`Scheduler::restore_whole`]).
    #[must_use]
    pub fn whole_mark(&self, operation: OperationId) -> Option<&WholeMark> {
        self.drain.marks.get(&operation)
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
    /// When the operation is placed on a worker whose host ends ([`crate::Event::HostEnd`]), the
    /// time is cut to the time left ([`Scheduler::max_wall`]).
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
            let end = self.end_times(held.operation, (start, self.lease_host_end(held.operation)));
            let holding = self.ops[&held.operation].state.holding();
            holding
                .into_iter()
                .for_each(|(_, worker)| ends.entry(worker).or_default().push(end));
        }
        for (operation, placed) in placed {
            let end = self.end_times(
                *operation,
                (self.now.as_millis(), self.host_end_ms(&placed.worker)),
            );
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
    fn end_times(
        &self,
        operation: OperationId,
        (start, host_end): (u64, u64),
    ) -> (u64, Option<u64>) {
        let bound = self.max_walls.get(&operation).map_or(u64::MAX, |wall| {
            start.saturating_add(u64::try_from(wall.as_millis()).unwrap_or(u64::MAX))
        });
        let bound = bound.min(host_end);
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
            (
                *lease,
                end(self.end_times(held.operation, (start, self.lease_host_end(held.operation)))),
            )
        });
        let now = self.now.as_millis();
        let this_round = (0u64..).zip(placed).map(|(i, (operation, placed))| {
            let lease = LeaseId::new(self.term, self.next_seq.saturating_add(i));
            (
                lease,
                end(self.end_times(*operation, (now, self.host_end_ms(&placed.worker)))),
            )
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
    /// never a held lease. A dropped mark bars no worker. The round's state for the caps.
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

    /// The worker whole-machine `id`, at queue `position`, holds this round, its mark and
    /// the start time of the hold for backfill, when it holds one. A mark belongs to its
    /// request until the request is placed or cancelled, past its deadline too: `kept`
    /// stays on its worker, without a check (that mark is already
    /// counted), for as long as that worker could still hold the request, however cheap
    /// another worker becomes to drain, and no other request's hold or mark takes the
    /// worker ([`Scheduler::off_marks`]). Only a worker that can no longer hold it (gone,
    /// cordoned, barred to it) moves the mark, keeping its age.
    /// A new mark is made only if the caps allow it ([`may_reserve`]) and the worker is
    /// not the last capable one of an older ordinary request, in which case the next
    /// worker by drain cost is tried. `Err` is the reason it holds none. `Ok(None)` is no
    /// worker that could.
    ///
    /// The start time is when the request is expected to start there (`None`: not known):
    /// the mark's tail end, or the worker's tail now if that ends sooner
    /// ([`backfill::kept`]); a known tail end is kept when the tail now is not known, and a
    /// mark made with no known tail end has no start, whatever the tail now, so nothing
    /// backfills under it.
    ///
    /// Head holds are not marks: `reserved` keeps a request off a worker a head holds, and
    /// no cap counts it. A worker a whole-machine lease holds, or a whole-machine request
    /// was placed on this round, is never marked: the lease is not touched, and the worker
    /// is counted once, as held.
    pub(super) fn hold_whole(
        &self,
        servable: &mut Servable,
        round: &Round,
        (id, position): (OperationId, u64),
        reserved: &BTreeSet<WorkerId>,
        placed: &[(OperationId, Placed)],
        turn: Turn,
    ) -> Result<Option<(WorkerId, WholeMark, Option<u64>)>, WholeWait> {
        let cfg = self
            .whole_node
            .as_ref()
            .expect("whole-node settings are on");
        let op = &self.ops[&id];
        let mut out: BTreeSet<WorkerId> = reserved.union(&turn.barred).cloned().collect();
        // Another request's mark is never taken: it stays on its worker.
        out.extend(self.marked_workers(id).cloned());
        // A worker a whole-machine lease holds, granted before or placed this round, is
        // whole already: it is never marked, so no request waits behind another one's
        // whole-machine lease (that lease is never touched) and the caps count the worker
        // once, as held.
        out.extend(self.whole_held().into_keys().cloned());
        let whole_placed = placed.iter().filter(|(operation, _)| {
            let op = self.ops.get(operation);
            op.is_some_and(|op| op.request.kind == LeaseKind::WholeMachine)
        });
        out.extend(whole_placed.map(|(_, at)| at.worker.clone()));
        let avoid = Drain::avoiding(&op.avoid, &turn.barred);
        let kept = turn.kept.as_ref().map(|m| &m.worker);
        let drain = self.drain_order(placed);
        let max_wall = self.max_walls.get(&id).copied();
        let mut wait = None;
        loop {
            // A mark stays on its worker while that worker could still hold the request:
            // it is not picked again when another worker becomes cheaper to drain. Its cap
            // check was made when it was made.
            let Some(name) =
                servable.reserve(&self.workers, &op.request, &out, &avoid, kept, &drain)
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
            let tail = super::gate::tail_of(&drain, &name);
            let mark = Drain::mark((&name, tail), turn.kept, self.now, cfg, max_wall);
            let mark = self.end_bounded(mark);
            let start = mark
                .tail_end
                .and_then(|end| backfill::kept(Some(end.as_millis()), tail));
            return Ok(Some((name, mark, start)));
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

    /// The workers the marks of requests other than `id` are on, of the marks this round
    /// has not reached yet: a request's own mark leaves them when the round reaches it
    /// ([`Drain::turn`]), and is kept or moved there.
    fn marked_workers(&self, id: OperationId) -> impl Iterator<Item = &WorkerId> + '_ {
        let others = self.drain.marks.iter().filter(move |(op, _)| **op != id);
        others.map(|(_, m)| &m.worker)
    }

    /// `reserved` with the workers [`Scheduler::marked_workers`] names for `id`, which
    /// picks a hold or places a whole-machine request: neither takes a worker another
    /// request's mark is on, so a mark is never pushed off its worker before its own
    /// request is reached. Ordinary work is placed there as before (work ahead of a
    /// request in queue order is never held back). `None` when there is no such mark, so
    /// `reserved` stands.
    pub(super) fn off_marks(
        &self,
        reserved: &BTreeSet<WorkerId>,
        id: OperationId,
    ) -> Option<BTreeSet<WorkerId>> {
        let mut marked = self.marked_workers(id).peekable();
        marked.peek()?;
        Some(reserved.iter().chain(marked).cloned().collect())
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

    /// Catches: a turn past the mark's deadline dropping the mark or barring its worker:
    /// the mark comes back whole, and the request is barred from nothing.
    #[test]
    fn a_turn_past_the_deadline_keeps_the_mark_and_bars_nothing() {
        let cfg = WholeNodeConfig::dedicated(4);
        let op = OperationId(1);
        let mark = WholeMark {
            worker: WorkerId::new("a"),
            made: FarmTime::from_millis(0),
            deadline: FarmTime::from_millis(10),
            tail_end: Some(FarmTime::from_millis(50)),
        };
        let mut drain = Drain::default();
        drain.marks.insert(op, mark.clone());
        let turn = drain
            .turn(
                op,
                FarmTime::from_millis(0),
                FarmTime::from_millis(1_000),
                &cfg,
            )
            .unwrap();
        assert_eq!(turn.kept, Some(mark));
        assert!(turn.barred.is_empty());
        assert!(drain.bars.is_empty());
    }

    /// Catches: `forget` leaving one of the maps behind: each holds an entry for the
    /// operation (a mark, a back-off and a wait), and none does after it, while another
    /// operation's mark stays.
    #[test]
    fn forget_drops_the_mark_the_backoff_and_the_wait() {
        let op = OperationId(1);
        let other = OperationId(2);
        let mark = WholeMark {
            worker: WorkerId::new("a"),
            made: FarmTime::from_millis(0),
            deadline: FarmTime::from_millis(10),
            tail_end: None,
        };
        let mut drain = Drain::default();
        drain.marks.insert(op, mark.clone());
        drain.marks.insert(other, mark);
        let until = FarmTime::from_millis(100);
        let now = FarmTime::from_millis(0);
        drain.bar(op, WorkerId::new("b"), Bar::Backoff(until), now);
        drain.waits.insert(op, WholeWait::LastCapable);
        drain.forget(op);
        assert!(!drain.marks.contains_key(&op));
        assert!(!drain.bars.contains_key(&op));
        assert!(!drain.waits.contains_key(&op));
        assert!(drain.marks.contains_key(&other));
    }

    /// Catches: a lease placed in the round being left out of the lease ends, or numbered
    /// wrongly: the one placed this round takes the next sequence of the term and ends its
    /// max wall time after now, cut to nothing else; with no estimate its end is its bound.
    #[test]
    fn a_lease_placed_this_round_ends_at_now_plus_its_wall_time() {
        use crate::{Event, Input, Request};
        use js_types::sched::usage::LeaseCost;
        use js_types::sched::{Qos, Resources, StateMachine, WaiterId, WorkKey};
        let mut s = Scheduler::new(1);
        let op = s.next_operation();
        let request = Request {
            key: WorkKey::new("c", "k"),
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, 1 << 30),
            hermetic: true,
            do_not_cache: false,
            whole_worker: false,
            pin: None,
            needs: js_caps::Request::default(),
        };
        let waiter = WaiterId(1);
        s.apply(Input::new(
            FarmTime::from_millis(5_000),
            Event::Submit { waiter, request },
        ));
        s.set_max_wall(op, Duration::from_secs(100));
        let placed = Placed {
            worker: WorkerId::new("a"),
            booked: Resources::new(1_000, 1 << 30),
            device: None,
            cost: LeaseCost {
                cpu_millis: 1_000,
                memory_bytes: 0,
                limit_bytes: 0,
                cold: false,
                retry: false,
                key_hash: 0,
            },
        };
        let ends = s.lease_ends(&[(op, placed)]);
        let expected = BTreeMap::from([(LeaseId::new(1, 0), Some(105_000))]);
        assert_eq!(ends, expected);
    }
}
