//! Forgetting a queued operation that another scheduler of the same state store took.
//!
//! Several schedulers over one state store each queue what the store queues, and each may
//! place it. The store decides which one does: the grant is a conditional write, and the
//! others lose it. A scheduler that learns from the store that an operation it holds queued
//! is no longer queued there (another scheduler granted it, finished it or refused it) drops
//! it, so that it places nothing the store gave to another.

use std::cmp::Reverse;

use js_types::sched::{OperationId, WaiterId};

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
        let op = self.ops.remove(&operation).expect("checked above");
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
        Some(op.waiters)
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
