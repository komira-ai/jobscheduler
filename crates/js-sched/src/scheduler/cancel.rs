//! Cancelling an operation in any state ([`Scheduler::cancel`]).
//!
//! A method of the scheduler, not an [`crate::Event`], as [`Scheduler::unplace`] is: the
//! caller learns of the cancel from outside the event stream (a client, its ledger) and
//! calls it under the lock that serialises its inputs.

use js_types::sched::usage::RunEnd;
use js_types::sched::{LeaseId, OperationId, WaiterId, WorkerId};

use super::Scheduler;

/// What [`Scheduler::cancel`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cancelled {
    /// The operation was queued: it left the queue, with the mark it had.
    Queued {
        /// Its callers, for the caller to answer.
        waiters: Vec<WaiterId>,
        /// The worker its whole-node mark was on, now free of it.
        mark: Option<WorkerId>,
    },
    /// The operation held `lease` on `worker`, leased or running: the lease is released
    /// there, and the caller stops it on the worker if its `Start` was sent.
    Held {
        /// The released lease.
        lease: LeaseId,
        /// The worker it was on.
        worker: WorkerId,
        /// Its callers, for the caller to answer.
        waiters: Vec<WaiterId>,
    },
    /// Nothing was cancelled: the operation is finished (kept for the finished
    /// retention), or the scheduler does not hold it (unknown, or cancelled already).
    Nothing,
}

impl Scheduler {
    /// Cancels `operation`, whatever its state, and forgets it as
    /// [`Scheduler::forget_queued`] does, recording nothing (no result, no give-up, no
    /// loss, no fault). A queued one leaves the queue, and its whole-node mark, give-up
    /// count and bars go in the same call, so its worker takes other work from the next
    /// placement round. A leased or running one has its lease released as a finished run's
    /// is: its worker's booking and lease count fall at once, so the next round may place
    /// on it, and a grant, start or result that comes for the lease later finds no
    /// operation and is dropped. A finished operation, or one the scheduler does not hold,
    /// is left as it is; so cancelling twice is cancelling once.
    ///
    /// The scheduler cannot stop a run: [`Cancelled::Held`] names the lease for the caller
    /// to stop on its worker, and the caller calls this when the worker may be booked again
    /// (for a whole-machine lease in prepare, once the worker leaves it).
    pub fn cancel(&mut self, operation: OperationId) -> Cancelled {
        let Some(op) = self.ops.get(&operation) else {
            return Cancelled::Nothing;
        };
        if op.state.is_done() {
            return Cancelled::Nothing;
        }
        let holding = op
            .state
            .holding()
            .map(|(lease, worker)| (lease, worker.clone()));
        let mark = self.drain.marks.get(&operation).map(|m| m.worker.clone());
        self.release(operation, RunEnd::Cancelled);
        let waiters = self.drop_operation(operation);
        match holding {
            Some((lease, worker)) => Cancelled::Held {
                lease,
                worker,
                waiters,
            },
            None => Cancelled::Queued { waiters, mark },
        }
    }
}
