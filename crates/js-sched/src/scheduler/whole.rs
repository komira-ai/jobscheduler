//! The drain cost of a worker, for the worker a blocked whole-machine request holds, and
//! the wait before it holds one, the deadline of the hold and its give-up
//! ([`Scheduler::with_whole_node`]).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_types::sched::{FarmTime, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::cost::{DrainCost, HostLoad, drain_cost, reserve_deadline};

use crate::scheduler::Scheduler;
use crate::servable::Placed;

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
}

/// What a request may do in one placement round, from [`Drain::turn`].
pub(super) struct Turn {
    /// The mark the request keeps, if its hold is still on.
    pub(super) kept: Option<WholeMark>,
    /// The workers the request may not hold now.
    pub(super) barred: BTreeSet<WorkerId>,
}

impl Drain {
    /// The workers `operation` avoids at `now`: those whose back-off has not ended.
    pub(super) fn barred(&self, operation: OperationId, now: FarmTime) -> BTreeSet<WorkerId> {
        self.backoff
            .get(&operation)
            .map_or_else(BTreeSet::new, |m| {
                m.iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(worker, _)| worker.clone())
                    .collect()
            })
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
            let given_up = self.give_ups.get(&operation).copied().unwrap_or(0);
            if now >= mark.deadline && given_up < cfg.max_reserve_attempts {
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
    /// same worker, else a new one whose deadline follows [`reserve_deadline`].
    pub(super) fn mark(
        &self,
        operation: OperationId,
        worker: &WorkerId,
        kept: Option<WholeMark>,
        now: FarmTime,
        cfg: &WholeNodeConfig,
        max_wall: Option<Duration>,
    ) -> WholeMark {
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
            made: now,
            deadline: FarmTime::from_millis(now.as_millis().saturating_add(wait)),
        }
    }

    /// Forgets `operation`.
    pub(super) fn forget(&mut self, operation: OperationId) {
        self.marks.remove(&operation);
        self.give_ups.remove(&operation);
        self.backoff.remove(&operation);
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

    /// Names the longest `operation` may run once leased, which the caller knows from the
    /// request's wall time: no scheduler input and no log record carries it. A lease's end
    /// bound is its start plus this; a lease whose operation has none is taken to end at
    /// the latest time there is, so a worker holding it drains last. Set it with the
    /// submit and again for an operation taken up after a restart (a recovered lease's
    /// start is the recovery, so its bound is that plus this). Dropped with the operation.
    pub fn set_max_wall(&mut self, operation: OperationId, max_wall: Duration) {
        self.max_walls.insert(operation, max_wall);
    }

    /// The registered workers by drain cost, cheapest first, when whole-node settings are
    /// on; `None` when they are not. `placed` are this round's placements, not yet held.
    pub(super) fn drain_order(&self, placed: &[(OperationId, Placed)]) -> Option<Vec<DrainCost>> {
        self.whole_node.as_ref()?;
        let end_of = |operation: OperationId, start: u64| {
            self.max_walls.get(&operation).map_or(u64::MAX, |wall| {
                start.saturating_add(u64::try_from(wall.as_millis()).unwrap_or(u64::MAX))
            })
        };
        // Every held lease belongs to an operation that holds it.
        let mut ends: BTreeMap<&WorkerId, Vec<u64>> = BTreeMap::new();
        for held in self.held.values() {
            let start = held.started_at.unwrap_or(held.granted_at).as_millis();
            let end = end_of(held.operation, start);
            let holding = self.ops[&held.operation].state.holding();
            holding
                .into_iter()
                .for_each(|(_, worker)| ends.entry(worker).or_default().push(end));
        }
        for (operation, placed) in placed {
            let end = end_of(*operation, self.now.as_millis());
            ends.entry(&placed.worker).or_default().push(end);
        }
        let loads: Vec<HostLoad> = self
            .workers
            .iter()
            .map(|(name, w)| HostLoad {
                id: name.as_str().to_owned(),
                leases: w.leases,
                end_bounds: ends.remove(name).unwrap_or_default(),
            })
            .collect();
        Some(drain_cost(&loads, self.now.as_millis()))
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
