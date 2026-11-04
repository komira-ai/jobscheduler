//! Forgetting a queued operation that another scheduler of the same state store took.
//!
//! Several schedulers over one state store each queue what the store queues, and each may
//! place it. The store decides which one does: the grant is a conditional write, and the
//! others lose it. A scheduler that learns from the store that an operation it holds queued
//! is no longer queued there (another scheduler granted it, finished it or refused it) drops
//! it, so that it places nothing the store gave to another. A worker whose host the caller
//! no longer lists is forgotten here too ([`Scheduler::forget_worker`]).

use std::cmp::Reverse;

use js_types::sched::{OperationId, WaiterId, WorkerId};

use super::{OpState, Scheduler};

impl Scheduler {
    /// Forgets `operation`, which this scheduler holds queued and another one took: it
    /// leaves the queue and every table that names it, and nothing is recorded (no give-up,
    /// no loss, no fault). Returns the callers it had, for the caller to answer from the
    /// state store when the other scheduler finishes it.
    ///
    /// `None` leaves everything as it was: the scheduler holds no such operation, or it is
    /// not queued (it holds a lease, or it is finished and kept for the finished retention).
    pub fn forget_queued(&mut self, operation: OperationId) -> Option<Vec<WaiterId>> {
        self.ops
            .get(&operation)
            .filter(|op| op.state == OpState::Queued)?;
        Some(self.drop_operation(operation))
    }

    /// Removes `operation` from the queue and every table that names it, recording
    /// nothing; returns the callers it had (none for one it does not hold). Its lease, if
    /// it holds one, is released first by the caller.
    pub(super) fn drop_operation(&mut self, operation: OperationId) -> Vec<WaiterId> {
        self.ops.remove(&operation).map_or_else(Vec::new, |op| {
            self.queue
                .remove(&(Reverse(op.request.qos.clone()), operation));
            if self.in_flight.get(&op.request.key) == Some(&operation) {
                self.in_flight.remove(&op.request.key);
            }
            self.reservations.remove(&operation);
            self.memory.forget(operation);
            self.size_keys.remove(&operation);
            self.max_walls.remove(&operation);
            self.drain.forget(operation);
            self.backfill.forget(operation);
            self.admission.forget(operation);
            op.waiters
        })
    }

    /// Forgets `worker`, whose host its caller no longer lists: from now on it is no worker
    /// of this scheduler, and its readings, its cordon and any hold put on it from outside
    /// ([`Scheduler::reserve_outside`]) go with it. Only a worker that holds no lease and
    /// for which no recovered lease waits is forgotten; answers whether it was. A worker
    /// this scheduler does not know is left as it is.
    pub fn forget_worker(&mut self, worker: &WorkerId) -> bool {
        let holds = self.held.values().any(|held| {
            let holder = self.ops[&held.operation].state.holding();
            holder.is_some_and(|(_, on)| on == worker)
        });
        if holds | self.awaited.contains_key(worker) {
            return false;
        }
        self.cordons.uncordon(worker);
        self.release_outside(worker);
        self.workers.remove(worker).is_some()
    }
}

#[cfg(test)]
mod tests {
    use js_types::sched::{FarmTime, LeaseKind, Qos, Resources, StateMachine};

    use super::*;
    use crate::{Event, Input, Prediction, Request};

    /// Catches: `forget_queued` leaving the operation's cost hint ([`Scheduler::set_cost`])
    /// in admission: the hint of a forgotten operation is gone, and another operation's stays.
    #[test]
    fn forgetting_a_queued_operation_drops_its_cost_hint() {
        let mut s = Scheduler::new(1);
        let mut submit = |waiter: u64, tag: u8| {
            let request = Request {
                key: js_types::sched::WorkKey::new("main", format!("action-{tag}")),
                qos: Qos::Ci,
                kind: LeaseKind::Action,
                resources: Resources::new(1_000, 1 << 30),
                hermetic: true,
                do_not_cache: false,
                whole_worker: false,
                pin: None,
                needs: js_caps::Request::default(),
            };
            let event = Event::Submit {
                waiter: WaiterId(waiter),
                request,
            };
            s.apply(Input::new(FarmTime::default(), event));
        };
        submit(1, 1);
        submit(2, 2);
        let hint = Prediction {
            cpu_millis: 500,
            memory_bytes: 1 << 20,
            cold: false,
            life_minutes: 3,
        };
        s.set_cost(OperationId(0), hint);
        s.set_cost(OperationId(1), hint);
        assert!(s.admission.hinted(OperationId(0)));

        assert_eq!(s.forget_queued(OperationId(0)), Some(vec![WaiterId(1)]));

        assert!(!s.admission.hinted(OperationId(0)));
        assert!(s.admission.hinted(OperationId(1)));
    }
}
