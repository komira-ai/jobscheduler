//! One view of why a whole-machine request waits ([`Scheduler::whole_state`]) and of the
//! numbers the caps use ([`Scheduler::whole_summary`]), for a caller that shows them
//! (a status page, a metric) without reading the accessors one by one.
//!
//! Both read what the placement rounds recorded, the marks, give-ups, back-offs and
//! waits ([`Scheduler::whole_mark`], [`Scheduler::whole_give_ups`],
//! [`Scheduler::whole_wait`]), and count with the code the caps use, so neither can
//! drift from what admission enforces.

use std::collections::BTreeMap;

use js_types::sched::{FarmTime, LeaseKind, OperationId, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::Blocked;

use super::whole::{WholeWait, ceiling_of};
use super::{OpState, Scheduler};

/// Why a queued whole-machine request is not yet granted a worker, with settings on
/// ([`Scheduler::whole_state`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WholeState {
    /// It has not waited out `drain_after`: it holds no worker and takes only one that is
    /// quiet.
    Early {
        /// When it may first hold a worker: its queue time plus `drain_after`.
        may_hold_at: FarmTime,
    },
    /// It holds a worker: no request after it in queue order is placed there, and the
    /// worker is granted to it when it has no lease.
    Holding {
        /// The held worker.
        worker: WorkerId,
        /// When the hold was made.
        made: FarmTime,
        /// When the hold is given up if the worker is still busy. `None` once the request
        /// has given up `max_reserve_attempts` holds: it is not given up again.
        deadline: Option<FarmTime>,
        /// How many holds it has given up so far.
        give_ups: u32,
        /// Whether the worker still has leases, so it is draining; when it has none the
        /// next round grants it.
        draining: bool,
    },
    /// It gave up a hold and holds no worker: the workers it gave up are barred until
    /// their back-off ends, and no other worker could be held.
    Barred {
        /// The barred workers, each with when its back-off ends (exclusive).
        workers: BTreeMap<WorkerId, FarmTime>,
        /// How many holds it has given up so far.
        give_ups: u32,
    },
    /// It has waited out `drain_after` and a cap, or the last-capable rule, stopped a
    /// hold ([`Scheduler::whole_wait`]).
    Stopped(WholeWait),
    /// It has waited out `drain_after`, holds nothing and no cap or give-up is the reason:
    /// the workers it could hold are held by requests ahead of it, or no placement round
    /// has run since it became due.
    Queued,
    /// No live worker could ever hold it; the reason is [`Scheduler::waiting`]'s.
    Unservable {
        /// Why no live worker can run it.
        reason: String,
    },
}

/// The kind of a [`WholeState`], for counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WholeReason {
    /// [`WholeState::Early`].
    Early,
    /// [`WholeState::Holding`].
    Holding,
    /// [`WholeState::Barred`].
    Barred,
    /// [`WholeState::Stopped`] by a cap.
    Cap(Blocked),
    /// [`WholeState::Stopped`] by the last-capable rule.
    LastCapable,
    /// [`WholeState::Queued`].
    Queued,
    /// [`WholeState::Unservable`].
    Unservable,
}

impl WholeState {
    /// The kind of this state.
    #[must_use]
    pub fn reason(&self) -> WholeReason {
        match self {
            Self::Early { .. } => WholeReason::Early,
            Self::Holding { .. } => WholeReason::Holding,
            Self::Barred { .. } => WholeReason::Barred,
            Self::Stopped(WholeWait::Cap(blocked)) => WholeReason::Cap(*blocked),
            Self::Stopped(WholeWait::LastCapable) => WholeReason::LastCapable,
            Self::Queued => WholeReason::Queued,
            Self::Unservable { .. } => WholeReason::Unservable,
        }
    }
}

/// What the caps of whole-node settings count and allow now ([`Scheduler::whole_summary`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeSummary {
    /// Workers held by a whole-machine lease.
    pub held: usize,
    /// Workers held by a queued request's mark.
    pub marks: usize,
    /// Workers whole: `held` plus `marks`, what `max_whole_node` and the ceiling bound.
    pub whole: usize,
    /// The marks on a worker that still has leases: what `max_draining` bounds.
    pub draining: usize,
    /// The most workers that may be whole: `max_whole_node`, or the capable hosts less
    /// `keep_for_ordinary` if that is fewer. It falls when workers leave the pool; held
    /// leases stay above it until they end, so `whole` can exceed it for a time.
    pub ceiling: usize,
    /// `max_whole_node` as set.
    pub max_whole_node: usize,
    /// The workers that serve whole-machine leases, less those leaving the pool
    /// (draining) or quarantined; a silent one still counts.
    pub capable_hosts: usize,
    /// `keep_for_ordinary` as set.
    pub keep_for_ordinary: usize,
    /// `max_draining` as set.
    pub max_draining: usize,
    /// The queued whole-machine requests by their reason, those with one at least.
    pub waiting: BTreeMap<WholeReason, usize>,
}

impl Scheduler {
    /// Why queued whole-machine `operation` is not yet granted a worker, with whole-node
    /// settings on ([`Scheduler::with_whole_node`]); `None` without them, or if
    /// `operation` is not a queued whole-machine request (unknown, cancelled, placed or
    /// finished). The marks, waits and back-offs are as of the last placement round;
    /// whether it is before `drain_after` is read at the scheduler's time now.
    #[must_use]
    pub fn whole_state(&self, operation: OperationId) -> Option<WholeState> {
        let cfg = self.whole_node.as_ref()?;
        let op = self.ops.get(&operation)?;
        if op.request.kind != LeaseKind::WholeMachine || op.state != OpState::Queued {
            return None;
        }
        Some(self.state_of(cfg, operation))
    }

    /// The caps' numbers now and how many queued whole-machine requests wait for each
    /// reason, with whole-node settings on; `None` without them. The counts are those
    /// admission reads.
    #[must_use]
    pub fn whole_summary(&self) -> Option<WholeSummary> {
        let cfg = self.whole_node.as_ref()?;
        let (limits, counts, held) = self.whole_at_rest(cfg);
        let mut waiting = BTreeMap::new();
        for operation in self.queued() {
            if self.ops[&operation].request.kind == LeaseKind::WholeMachine {
                *waiting
                    .entry(self.state_of(cfg, operation).reason())
                    .or_default() += 1;
            }
        }
        Some(WholeSummary {
            held,
            marks: counts.whole - held,
            whole: counts.whole,
            draining: counts.reserved,
            ceiling: ceiling_of(limits),
            max_whole_node: limits.max_whole_node,
            capable_hosts: limits.capable_hosts,
            keep_for_ordinary: limits.keep_for_ordinary,
            max_draining: limits.max_draining,
            waiting,
        })
    }

    /// The state of queued whole-machine `operation`, in the order the placement round
    /// decides: unservable, before its turn, holding, stopped, barred, else queued.
    fn state_of(&self, cfg: &WholeNodeConfig, operation: OperationId) -> WholeState {
        let op = &self.ops[&operation];
        if let Some(unservable) = &op.unservable {
            return WholeState::Unservable {
                reason: unservable.reason.clone(),
            };
        }
        let may_hold_at =
            FarmTime::from_millis(op.queued_at.as_millis().saturating_add(cfg.drain_after_ms));
        if self.now < may_hold_at {
            return WholeState::Early { may_hold_at };
        }
        let give_ups = self.whole_give_ups(operation);
        if let Some(mark) = self.whole_mark(operation) {
            return WholeState::Holding {
                worker: mark.worker.clone(),
                made: mark.made,
                deadline: self.drain.gives_up(operation, cfg).then_some(mark.deadline),
                give_ups,
                draining: self.workers.get(&mark.worker).is_some_and(|w| w.leases > 0),
            };
        }
        if let Some(wait) = self.whole_wait(operation) {
            return WholeState::Stopped(wait);
        }
        let workers = self.drain.barred_until(operation, self.now);
        if workers.is_empty() {
            WholeState::Queued
        } else {
            WholeState::Barred { workers, give_ups }
        }
    }
}
