//! Applying committed records: a refusal, a lease grant, a start and a result, and the
//! report that proposes a result.

use std::cmp::Reverse;

use js_types::sched::{
    Answer, ControlRecord, Effect, Failure, LeaseGrant, LeaseId, OperationId, Outcome, Refusal,
    RefusalRecord, ResultRecord, StartLease,
};

use super::{OpState, Scheduler, StartSent, run_end_of};

impl Scheduler {
    /// A committed refusal: the operation is finished, and its waiters are answered.
    /// A refusal of an operation that is no longer queued is stale and dropped.
    pub(super) fn refusal_committed(&mut self, record: &RefusalRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        if op.state != OpState::Queued {
            return Vec::new();
        }
        op.state = OpState::Refused {
            reason: record.reason.clone(),
        };
        op.unservable = None;
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        self.finished.push_back((self.now, id));
        vec![Effect::Refuse(Refusal {
            operation: id,
            waiters: op.waiters.clone(),
            reason: record.reason.clone(),
        })]
    }

    pub(super) fn lease_committed(&mut self, grant: &LeaseGrant, effects: &mut Vec<Effect>) {
        let Some(op) = self.ops.get_mut(&grant.operation) else {
            return;
        };
        if op.state.is_done() {
            return;
        }
        // Only the operation's current lease is its committed one: lease numbers come in
        // blocks per frontend, so a larger one is not a later one, and a grant that is no
        // longer current must not shadow the current lease's result.
        if let OpState::Leased {
            lease,
            worker,
            committed,
        } = &mut op.state
            && *lease == grant.lease
            && !*committed
        {
            *committed = true;
            let (session, process) = self
                .workers
                .get(&*worker)
                .map_or((0, 0), |w| (w.session, w.process));
            let sent = StartSent {
                at: self.now,
                session,
                process,
            };
            // A leased operation's lease is always held: no branch on it.
            let (booked, device) =
                self.held
                    .get_mut(&*lease)
                    .map_or((op.request.resources, None), |held| {
                        held.start_sent = Some(sent);
                        (held.booked, held.device.clone())
                    });
            effects.push(Effect::Start(StartLease {
                worker: worker.clone(),
                lease: *lease,
                operation: grant.operation,
                key: op.request.key.clone(),
                kind: op.request.kind,
                resources: booked,
                fence: op.request.fence(),
                device,
            }));
        }
    }

    pub(super) fn started(&mut self, id: OperationId, lease: LeaseId) {
        let Some(op) = self.ops.get_mut(&id) else {
            return;
        };
        if let OpState::Leased {
            lease: held,
            worker,
            committed: true,
        } = &op.state
            && *held == lease
        {
            op.state = OpState::Running {
                lease,
                worker: worker.clone(),
            };
            // A leased operation's lease is always held: no branch on it.
            let now = self.now;
            let () = self
                .held
                .get_mut(&lease)
                .map_or((), |held| held.started_at = Some(now));
        }
    }

    pub(super) fn report(
        &mut self,
        id: OperationId,
        lease: LeaseId,
        outcome: Outcome,
    ) -> Vec<Effect> {
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        let current = match &op.state {
            OpState::Leased {
                lease, committed, ..
            } => committed.then_some(*lease),
            OpState::Running { lease, .. } => Some(*lease),
            _ => None,
        };
        if current != Some(lease) || op.result_proposed {
            return Vec::new();
        }
        op.result_proposed = true;
        vec![Effect::Commit(ControlRecord::Result(ResultRecord {
            lease,
            operation: id,
            outcome,
        }))]
    }

    pub(super) fn result_committed(&mut self, record: &ResultRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get(&id) else {
            return Vec::new();
        };
        if op.state.is_done() || op.committed_lease != Some(record.lease) {
            return Vec::new();
        }
        if let Outcome::Failed(kill @ (Failure::OutOfMemory | Failure::NodeMemoryPressure)) =
            record.outcome
            && self.rerun_after_memory_kill(record, kill)
        {
            return Vec::new();
        }
        if let Outcome::Failed(Failure::Lost(why)) = record.outcome
            && self.rerun_after_lost(record, why)
        {
            return Vec::new();
        }
        // The scheduler's own give-up for a lease that never started is not a run to charge.
        if record.outcome == Outcome::Failed(Failure::Infra)
            && !self.ops[&id].gave_up
            && self.rerun_after_infra(record)
        {
            return Vec::new();
        }
        self.release(id, run_end_of(record.outcome));
        let op = self.ops.get_mut(&id).expect("checked above");
        op.state = match record.outcome {
            Outcome::Completed { action_result } => OpState::Completed {
                lease: record.lease,
                action_result,
            },
            Outcome::Failed(failure) => OpState::Failed {
                lease: record.lease,
                failure,
            },
        };
        // A queued operation (expired, not yet re-granted) leaves the queue too.
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        self.finished.push_back((self.now, id));
        vec![Effect::Answer(Answer {
            operation: id,
            lease: record.lease,
            waiters: op.waiters.clone(),
            outcome: record.outcome,
            memory_runs: self.memory.runs(id).to_vec(),
            farm_runs: op.farm_runs.clone(),
        })]
    }
}
