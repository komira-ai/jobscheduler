//! Putting back the whole-node state of an operation that a caller keeps outside the
//! scheduler ([`Scheduler::restore_whole`]), and the hard bars a caller sets
//! ([`Scheduler::bar_whole`]).
//!
//! Both are methods of the scheduler, not [`crate::Event`]s: see the module docs of
//! `whole`.

use std::collections::BTreeMap;

use js_types::sched::{FarmTime, LeaseKind, OperationId, WorkerId};

use super::{Bar, WholeMark};
use crate::scheduler::{OpState, Operation, Scheduler};

/// The whole-node state of one operation as a caller kept it outside the scheduler (its
/// ledger, a store), for [`Scheduler::restore_whole`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredWhole {
    /// The operation: a whole-machine request the scheduler holds (recovered or
    /// submitted).
    pub operation: OperationId,
    /// When it entered the queue: `drain_after` counts from it.
    pub queued_at: FarmTime,
    /// The workers barred to it, and how ([`Scheduler::whole_barred`]).
    pub bars: BTreeMap<WorkerId, Bar>,
    /// The mark it has, if it is queued and holds a worker.
    pub mark: Option<RestoredMark>,
}

/// The mark of a [`RestoredWhole`]: the fields of [`WholeMark`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredMark {
    /// The held worker.
    pub worker: WorkerId,
    /// When the mark was made.
    pub made: FarmTime,
    /// When the worker is expected to be drained at the latest ([`WholeMark::deadline`]).
    pub deadline: FarmTime,
    /// The end of the worker's tail when the mark was made (`None`: not known).
    pub tail_end: Option<FarmTime>,
}

/// Why [`Scheduler::restore_whole`] or [`Scheduler::bar_whole`] changed nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WholeError {
    /// The scheduler has no whole-node settings ([`Scheduler::with_whole_node`]), so it
    /// keeps no whole-node state.
    #[error("whole-node settings are off")]
    Off,
    /// The scheduler holds no such operation.
    #[error("{0} is not an operation this scheduler holds")]
    UnknownOperation(OperationId),
    /// The operation is not a whole-machine request.
    #[error("{0} is not a whole-machine request")]
    NotWholeMachine(OperationId),
    /// A mark was given for an operation that is not queued.
    #[error("{0} is not queued, so it holds no mark")]
    NotQueued(OperationId),
    /// A mark was given on a worker that has not registered.
    #[error("{operation} cannot mark {worker}: the worker is not registered")]
    UnknownWorker {
        /// The operation the mark is for.
        operation: OperationId,
        /// The worker.
        worker: WorkerId,
    },
    /// A mark was given on a worker another operation marks.
    #[error("{operation} cannot mark {worker}: {by} marks it")]
    Marked {
        /// The operation the mark is for.
        operation: OperationId,
        /// The worker.
        worker: WorkerId,
        /// The operation whose mark is on it.
        by: OperationId,
    },
    /// A mark was given on a worker the operation is barred from.
    #[error("{operation} cannot mark {worker}: the worker is barred to it")]
    Barred {
        /// The operation the mark is for.
        operation: OperationId,
        /// The worker.
        worker: WorkerId,
    },
}

impl Scheduler {
    /// Puts back the whole-node state `state` names for an operation this scheduler holds
    /// (one taken up with [`Scheduler::recover`] or [`Scheduler::take_in`], or submitted):
    /// its queue time, its bars and its mark, each replacing what the scheduler has for
    /// it. A restart then neither restarts `drain_after` nor lifts a back-off or a bar,
    /// and a standing mark keeps its deadline and its tail end: the next round's backfill
    /// start on the worker is the restored tail end (or the tail now, if that ends
    /// sooner), never the start the mark it replaces held by. The next placement round
    /// reads them as it reads its own: the mark is counted by the caps and kept while its
    /// worker could hold the request, past its deadline too.
    ///
    /// Refused, changing nothing, without whole-node settings ([`WholeError::Off`]), for
    /// an operation it does not hold or that is not a whole-machine request, and for a mark
    /// of an operation that is not queued, on a worker that has not registered, on one
    /// another operation marks, or on one the restored bars bar at the scheduler's time
    /// now. A bar may name a worker that has not registered: it applies once it does.
    pub fn restore_whole(&mut self, state: RestoredWhole) -> Result<(), WholeError> {
        let RestoredWhole {
            operation,
            queued_at,
            bars,
            mark,
        } = state;
        let op = self.whole_operation(operation)?;
        if let Some(mark) = &mark {
            self.markable(operation, op, mark, &bars)?;
        }
        self.ops
            .entry(operation)
            .and_modify(|op| op.queued_at = queued_at);
        self.drain.restore(operation, bars);
        // The start a hold kept from the mark it replaces is not the restored mark's.
        self.backfill.forget(operation);
        self.drain.marks.remove(&operation);
        self.drain.marks.extend(mark.map(|mark| {
            let mark = WholeMark {
                worker: mark.worker,
                made: mark.made,
                deadline: mark.deadline,
                tail_end: mark.tail_end,
            };
            (operation, mark)
        }));
        Ok(())
    }

    /// Bars `worker` to whole-machine `operation` with `bar` (the worker keeps the
    /// stronger bar, [`Bar`]): from the next placement round the operation does not mark
    /// it, and, unless `bar` is a [`Bar::Backoff`], is not granted it either, quiet or not,
    /// even when it is the last worker that could run the operation (it then waits, and is
    /// not refused for it). A mark the operation has on `worker` is dropped at once. Other operations are not affected, and neither is
    /// [`crate::Fate::avoid`], which stays a preference. The worker need not have
    /// registered.
    ///
    /// For a caller that learns from outside that the worker must not run the operation,
    /// as when a prepare failed there. Refused, changing nothing, without whole-node
    /// settings and for an operation it does not hold or that is not a whole-machine
    /// request.
    pub fn bar_whole(
        &mut self,
        operation: OperationId,
        worker: WorkerId,
        bar: Bar,
    ) -> Result<(), WholeError> {
        self.whole_operation(operation)?;
        let now = self.now;
        let marked = self.drain.marks.get(&operation).map(|m| m.worker.clone());
        if bar.holds_at(now) && marked.as_ref() == Some(&worker) {
            self.drain.marks.remove(&operation);
        }
        self.drain.bar(operation, worker, bar, now);
        Ok(())
    }

    /// The workers barred to `operation` at `now`, each with its bar: those set with
    /// [`Scheduler::bar_whole`] or restored, those that have not ended. Empty for an operation the scheduler holds no bar for.
    #[must_use]
    pub fn whole_barred(&self, operation: OperationId, now: FarmTime) -> BTreeMap<WorkerId, Bar> {
        self.drain.bars_at(operation, now)
    }

    /// `operation`, if whole-node settings are on and it is a whole-machine request this
    /// scheduler holds.
    fn whole_operation(&self, operation: OperationId) -> Result<&Operation, WholeError> {
        if self.whole_node.is_none() {
            return Err(WholeError::Off);
        }
        let op = self
            .ops
            .get(&operation)
            .ok_or(WholeError::UnknownOperation(operation))?;
        if op.request.kind != LeaseKind::WholeMachine {
            return Err(WholeError::NotWholeMachine(operation));
        }
        Ok(op)
    }

    /// Whether `operation` (`op`), barred as `bars` says, may hold `mark`.
    fn markable(
        &self,
        operation: OperationId,
        op: &Operation,
        mark: &RestoredMark,
        bars: &BTreeMap<WorkerId, Bar>,
    ) -> Result<(), WholeError> {
        let worker = || mark.worker.clone();
        if op.state != OpState::Queued {
            return Err(WholeError::NotQueued(operation));
        }
        if !self.workers.contains_key(&mark.worker) {
            return Err(WholeError::UnknownWorker {
                operation,
                worker: worker(),
            });
        }
        let other = self
            .drain
            .marks
            .iter()
            .find(|(by, m)| **by != operation && m.worker == mark.worker);
        if let Some((by, _)) = other {
            return Err(WholeError::Marked {
                operation,
                worker: worker(),
                by: *by,
            });
        }
        if bars
            .get(&mark.worker)
            .is_some_and(|bar| bar.holds_at(self.now))
        {
            return Err(WholeError::Barred {
                operation,
                worker: worker(),
            });
        }
        Ok(())
    }
}
