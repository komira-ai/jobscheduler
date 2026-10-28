//! What the scenarios read back from the checker: the shadow's queue, bookings, charges and
//! holds. A module of the checker, for its private state.

use js_types::sched::{OperationId, Qos, WaiterId, WorkerId};

use super::Checker;

impl Checker {
    /// Whether every operation is finished.
    pub fn all_done(&self) -> bool {
        self.ops.values().all(|op| op.state.done())
    }

    /// Operations submitted so far.
    pub fn op_count(&self) -> u64 {
        self.next_op
    }

    /// Whether `id` is queued (in the shadow's queue).
    pub fn is_queued(&self, id: OperationId) -> bool {
        self.ops.get(&id).is_some_and(Self::queued)
    }

    /// The operation `waiter` is attached to.
    pub fn op_of(&self, waiter: WaiterId) -> Option<OperationId> {
        self.waiter_op.get(&waiter).copied()
    }

    /// `id`'s QoS level now.
    pub fn qos_of(&self, id: OperationId) -> &Qos {
        &self.ops[&id].qos
    }

    /// The cores `worker` is charged and the most it is planned to (load rule).
    pub fn charge_room(&self, worker: &WorkerId) -> (u64, u64) {
        let host = self.host(worker);
        (host.left(), host.cpu_limit())
    }

    /// Whether a blocked head held `worker` after the last round (load rule).
    pub fn is_held(&self, worker: &WorkerId) -> bool {
        self.holds.values().any(|w| w == worker)
    }

    /// The worker the head `id` held after the last round (load rule).
    pub fn holder_of(&self, id: OperationId) -> Option<&WorkerId> {
        self.holds.get(&id)
    }

    /// What is booked on `worker` and its capacity, as (millicores, bytes, GPUs).
    pub fn room(&self, worker: &WorkerId) -> ([u64; 3], [u64; 3]) {
        let w = &self.workers[worker];
        (w.booked, w.capacity)
    }

    /// The queued operations, most urgent first.
    pub fn queue(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.queue.iter().map(|(_, _, id)| *id)
    }

    /// `id`'s request vector, as (millicores, bytes, GPUs).
    pub fn resources_of(&self, id: OperationId) -> [u64; 3] {
        self.ops[&id].resources
    }
}
