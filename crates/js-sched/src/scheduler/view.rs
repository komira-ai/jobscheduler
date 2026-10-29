//! The scheduler's read-only view: what it knows of an operation, a worker and the
//! queue, for the caller that carries out its effects.

use std::collections::{BTreeMap, BTreeSet};

use js_types::sched::usage::LeaseCost;
use js_types::sched::{
    FarmRun, LeaseId, NodePhase, OperationId, Qos, Resources, WaiterId, WorkerId,
};

use super::{OpState, Scheduler};
use crate::admission::{LoadCounters, LoadView, Prediction};
use crate::cordon::Cordon;
use crate::input::Request;
use crate::unservable::Unservable;

impl Scheduler {
    /// Why no live worker can run `operation` now, while it is queued and none can.
    #[must_use]
    pub fn waiting(&self, operation: OperationId) -> Option<&str> {
        let op = self.ops.get(&operation)?;
        op.unservable.as_ref().map(|u| u.text.as_str())
    }

    /// [`Scheduler::waiting`] as a value: the cause, to match on, count or group by. Its
    /// `Display` is the text `waiting` returns.
    #[must_use]
    pub fn unservable(&self, operation: OperationId) -> Option<&Unservable> {
        let op = self.ops.get(&operation)?;
        op.unservable.as_ref().map(|u| &u.reason)
    }

    /// The request `operation` was submitted with, if the scheduler holds it.
    #[must_use]
    pub fn request(&self, operation: OperationId) -> Option<&Request> {
        self.ops.get(&operation).map(|op| &op.request)
    }

    /// The cordoned (or not serving) live workers that could run `operation` if placement
    /// could use them: the ones a "every worker that can run it is cordoned" reason names.
    /// Empty when `operation` is unknown or no such worker could.
    #[must_use]
    pub fn cordoned_could(&self, operation: OperationId) -> Vec<WorkerId> {
        let Some(op) = self.ops.get(&operation) else {
            return Vec::new();
        };
        crate::servable::Servable::cordoned_could(
            &self.workers,
            &self.cordons,
            self.now,
            &op.request,
        )
    }

    /// Where `operation` is, if it exists: it was submitted, and is unfinished or
    /// finished less than the finished retention ago.
    #[must_use]
    pub fn state(&self, operation: OperationId) -> Option<&OpState> {
        self.ops.get(&operation).map(|op| &op.state)
    }

    /// The runs of `operation` that ended as the farm's fault, oldest first; `None` once
    /// the scheduler no longer holds it.
    #[must_use]
    pub fn farm_runs(&self, operation: OperationId) -> Option<&[FarmRun]> {
        self.ops.get(&operation).map(|op| op.farm_runs.as_slice())
    }

    /// The workers placement skips for each operation that has any: the ones its runs
    /// ended on as the farm's fault, and the ones that dropped its `Start`.
    #[must_use]
    pub fn avoided(&self) -> BTreeMap<OperationId, BTreeSet<WorkerId>> {
        self.ops
            .iter()
            .filter(|(_, op)| !op.avoid.is_empty())
            .map(|(id, op)| (*id, op.avoid.clone()))
            .collect()
    }

    /// The operation in flight that a submission of `request` would join: its
    /// running twin, if `request` may join one.
    #[must_use]
    pub fn twin(&self, request: &Request) -> Option<OperationId> {
        if !request.joinable() {
            return None;
        }
        self.in_flight.get(&request.key).copied()
    }

    /// The id the next operation that joins no twin gets.
    #[must_use]
    pub const fn next_operation(&self) -> OperationId {
        OperationId(self.next_op)
    }

    /// Every operation not finished, in id order: the ones a successor process takes
    /// over, and the ones whose callers are still waiting.
    pub fn unfinished(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.ops
            .iter()
            .filter(|(_, op)| !op.state.is_done())
            .map(|(id, _)| *id)
    }

    /// The waiters attached to `operation`, in attach order.
    #[must_use]
    pub fn waiters(&self, operation: OperationId) -> Option<&[WaiterId]> {
        self.ops.get(&operation).map(|op| op.waiters.as_slice())
    }

    /// The size key `operation` was given ([`Scheduler::set_size_key`]), if it has one.
    #[must_use]
    pub fn size_key(&self, operation: OperationId) -> Option<u64> {
        self.size_keys.get(&operation).copied()
    }

    /// The QoS level `operation` is queued at (raised when a more urgent caller joins).
    #[must_use]
    pub fn qos(&self, operation: OperationId) -> Option<&Qos> {
        self.ops.get(&operation).map(|op| &op.request.qos)
    }

    /// The leader term this scheduler grants leases under.
    #[must_use]
    pub const fn term(&self) -> u64 {
        self.term
    }

    /// How many operations wait for a lease.
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// How many leases are held (granted or running), on all workers.
    #[must_use]
    pub fn leases_held(&self) -> usize {
        self.held.len()
    }

    /// Queued operations in the order placement will consider them.
    pub fn queued(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.queue.iter().map(|(_, id)| *id)
    }

    /// Whether `worker` is cordoned, and where its drain is; `None` while placement
    /// may use it.
    #[must_use]
    pub fn cordon(&self, worker: &WorkerId) -> Option<&Cordon> {
        self.cordons.get(worker)
    }

    /// The leases `worker` holds (granted or running), in lease order.
    #[must_use]
    pub fn leases_on(&self, worker: &WorkerId) -> Vec<LeaseId> {
        self.held
            .iter()
            .filter(|(_, h)| {
                self.ops[&h.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            })
            .map(|(lease, _)| *lease)
            .collect()
    }

    /// What `worker`'s newest `Hello` says it is doing ([`crate::Event::Phase`]), if it is
    /// registered.
    #[must_use]
    pub fn phase(&self, worker: &WorkerId) -> Option<&NodePhase> {
        self.workers.get(worker).map(|w| &w.phase)
    }

    /// The worker queued whole-machine `operation` holds while it waits for it to empty,
    /// if any.
    #[must_use]
    pub fn reservation(&self, operation: OperationId) -> Option<&WorkerId> {
        self.reservations.get(&operation)
    }

    /// What is booked on `worker`, if it is registered. Its memory is what the leases'
    /// limits add up to: a record, which admission does not read (an action's memory is
    /// the limit it first runs under).
    #[must_use]
    pub fn booked(&self, worker: &WorkerId) -> Option<Resources> {
        self.workers.get(worker).map(|w| w.booked)
    }

    /// Feeds the estimator's prediction for `operation`: what placement charges the lease
    /// it gets ([`Prediction`]). Not in the request, the log or the `Eq` memory state: the
    /// caller sets it again for a queued operation after a restart, and a lease placed
    /// keeps the cost it was charged ([`Scheduler::lease_cost`]) whatever is set later.
    pub fn set_cost(&mut self, operation: OperationId, prediction: Prediction) {
        self.admission.set_prediction(operation, prediction);
    }

    /// Whether a prediction set with [`Scheduler::set_cost`] is held for `operation`: it is
    /// dropped when the operation is forgotten as queued or retired.
    #[must_use]
    pub fn has_cost(&self, operation: OperationId) -> bool {
        self.admission.hinted(operation)
    }

    /// What admission charged `lease`, which a worker holds: the cost in its grant record.
    #[must_use]
    pub fn lease_cost(&self, lease: LeaseId) -> Option<LeaseCost> {
        let held = self.held.get(&lease)?;
        Some(held.cost)
    }

    /// What admission has done since the scheduler was built.
    #[must_use]
    pub fn load_counters(&self) -> LoadCounters {
        let mut counters = self.admission.counters;
        if self.admission.by_load() {
            let limited = self
                .workers
                .values()
                .filter(|w| w.alive(self.now) && w.load_state.cpu_limited())
                .count();
            counters.fallback_cpu_limited = u64::try_from(limited).unwrap_or(u64::MAX);
        }
        counters
    }

    /// How `worker` is charged as of the newest input, if it is registered.
    #[must_use]
    pub fn load_view(&self, worker: &WorkerId) -> Option<LoadView> {
        let w = self.workers.get(worker)?;
        Some(LoadView::of(
            &w.load_state,
            &w.capacity,
            self.now,
            &self.admission.cfg,
        ))
    }

    /// What `worker` is charged `minutes` minutes ahead, as `(millicores, bytes)`, before a
    /// new lease: the predicted load curve admission checks, summed over its leases. Past
    /// the horizon ([`crate::HORIZON_MINUTES`]) it is what each lease asks for from the
    /// horizon on. A worker charged by prediction alone is charged the same at every minute.
    #[must_use]
    pub fn charged_ahead(&self, worker: &WorkerId, minutes: u32) -> Option<(u64, u64)> {
        let slot = if minutes > crate::HORIZON_MINUTES {
            crate::admission::H_PLUS_SLOT
        } else {
            usize::try_from(minutes).unwrap_or(0)
        };
        self.load_at(worker, slot)
    }

    /// The predicate's left side at `slot` for `worker`, without a new lease: the CPU in
    /// millicores and the memory in bytes it charges the worker's leases and measurement
    /// then. A slot is `0..=`[`crate::HORIZON`] for that many minutes from now, or
    /// [`crate::H_PLUS`] for what each lease asks for from the horizon on; `None` for a
    /// worker that is not registered or a slot past [`crate::H_PLUS`]. A worker charged by
    /// prediction alone is charged the same at every slot.
    #[must_use]
    pub fn load_at(&self, worker: &WorkerId, slot: usize) -> Option<(u64, u64)> {
        let w = self.workers.get(worker)?;
        (slot <= crate::admission::H_PLUS_SLOT).then(|| {
            w.load_state
                .left_at(slot, self.now, &w.capacity, &self.admission.cfg)
        })
    }

    /// What the new lease of `operation` is charged at `slot` if placed on `worker`, as
    /// the predicate counts it: the CPU in millicores and the memory in bytes. By its
    /// key's curve at the slot where the lease is charged by one; the same flat figures at
    /// every slot for a lease charged flat, on a worker charged by prediction alone (its
    /// cores are then at least what a lease is charged there) and for an action that would
    /// own the worker whole. Both are clamped to the worker's capacity. Slots are those of
    /// [`Scheduler::load_at`]. `None` for an operation the scheduler does not hold, a worker
    /// that is not registered or a slot past [`crate::H_PLUS`].
    ///
    /// The predicate checks memory at slot 0 only (the memory it charges never rises with
    /// the minute); the figure at a later slot is the lease's charge then.
    #[must_use]
    pub fn charge_at(
        &self,
        operation: OperationId,
        worker: &WorkerId,
        slot: usize,
    ) -> Option<(u64, u64)> {
        let request = &self.ops.get(&operation)?.request;
        let w = self.workers.get(worker)?;
        if slot > crate::admission::H_PLUS_SLOT {
            return None;
        }
        let (cost, profile) = self.charging(operation, request);
        let left = w
            .load_state
            .left(self.now, &w.capacity, &self.admission.cfg);
        Some(crate::servable::charge_on(
            w,
            request,
            cost,
            profile.as_ref(),
            &left,
            slot,
        ))
    }

    /// The runs left that the shape guard charges a key's CPU and memory flat, as
    /// `(cpu, memory)`: 0 for a resource that is charged by its curve. The key is the size
    /// key the caller named for the operation ([`Scheduler::set_size_key`]), else the
    /// [`js_types::sched::usage::key_hash`] of its request's key.
    #[must_use]
    pub fn shape_demoted_runs(&self, guard_key: u64) -> (u32, u32) {
        self.admission.shape_runs_left(guard_key)
    }

    /// The worker a blocked head of the queue, `operation`, holds for headroom, if any.
    #[must_use]
    pub fn head_hold(&self, operation: OperationId) -> Option<&WorkerId> {
        self.head_holds.get(&operation)
    }

    /// How many live registered workers are charged by prediction alone (no usable
    /// measurement) as of the newest input.
    #[must_use]
    pub fn fallback_workers(&self) -> usize {
        self.workers
            .values()
            .filter(|w| w.alive(self.now))
            .filter(|w| !w.load_state.fresh(self.now, &self.admission.cfg))
            .count()
    }

    /// What `worker` reported it can run at once, if it is registered: its newest
    /// `Hello`'s capacity. [`Scheduler::booked`] is what its leases hold of it.
    #[must_use]
    pub fn capacity(&self, worker: &WorkerId) -> Option<Resources> {
        self.workers.get(worker).map(|w| w.capacity)
    }

    /// The leases in `running` (a heartbeat's running set) that this scheduler granted
    /// and no longer holds on `worker`: given up, granted elsewhere, or finished. Their
    /// results can no longer be accepted, so a run of one is wasted, and a run of one
    /// whose operation was granted again runs beside the retry. The caller cancels
    /// them. A lease of another term is never named: this scheduler does not know
    /// whether a newer leader granted it.
    pub fn not_held<'a>(
        &'a self,
        worker: &'a WorkerId,
        running: &'a [LeaseId],
    ) -> impl Iterator<Item = LeaseId> + 'a {
        running.iter().copied().filter(move |lease| {
            let granted = lease.term == self.term && lease.seq < self.next_seq;
            let held_here = self.held.get(lease).is_some_and(|held| {
                self.ops[&held.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            });
            granted && !held_here
        })
    }

    /// How many of `operation`'s leases were lost so far ([`RequeueReason::is_loss`]);
    /// `None` once the scheduler no longer holds it.
    ///
    /// [`RequeueReason::is_loss`]: crate::RequeueReason::is_loss
    #[must_use]
    pub fn losses(&self, operation: OperationId) -> Option<u32> {
        self.ops.get(&operation).map(|op| op.losses)
    }
}
