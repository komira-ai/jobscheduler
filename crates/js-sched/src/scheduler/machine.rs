//! The scheduler's [`StateMachine`] implementation: one event in, the effects out.

use std::collections::BTreeSet;

use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, NodePhase, RefusalRecord, Resources, ResultRecord,
    StateMachine,
};

use super::{HANDOVER_GRACE, Scheduler, Worker};
use crate::admission::WorkerLoad;
use crate::input::{Event, Input};

/// The grant, if `record` is a lease record.
const fn lease_grant(record: &ControlRecord) -> Option<&LeaseGrant> {
    match record {
        ControlRecord::Lease(grant) => Some(grant),
        _ => None,
    }
}

/// The result, if `record` is a result record.
const fn result_record(record: &ControlRecord) -> Option<&ResultRecord> {
    match record {
        ControlRecord::Result(result) => Some(result),
        _ => None,
    }
}

/// The refusal, if `record` is a refusal record.
const fn refusal_record(record: &ControlRecord) -> Option<&RefusalRecord> {
    match record {
        ControlRecord::Refusal(refusal) => Some(refusal),
        _ => None,
    }
}

impl Scheduler {
    /// What the log committing `record` does. A record of another core of the log (jobs,
    /// alerts) is none of the kinds below and is not the scheduler's: it does nothing.
    /// Each kind is looked for on its own, so that the record of another kind is the
    /// case that the others share.
    fn committed(&mut self, record: &ControlRecord) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(grant) = lease_grant(record) {
            self.lease_committed(grant, &mut effects);
        }
        if let Some(result) = result_record(record) {
            effects.extend(self.result_committed(result));
        }
        if let Some(refusal) = refusal_record(record) {
            effects.extend(self.refusal_committed(refusal));
        }
        effects
    }
}

impl StateMachine for Scheduler {
    type Input = Input;

    fn apply(&mut self, input: Input) -> Vec<Effect> {
        self.now = self.now.max(input.now);
        let effects = match input.event {
            Event::WorkerUp {
                worker,
                instance,
                predecessor_stopped,
                capacity,
                caps,
            } => {
                let raw_memory_bytes = capacity.memory_bytes;
                let capacity = self.memory_percent.of_capacity(capacity);
                let now = self.now;
                match self.workers.get_mut(&worker) {
                    Some(w) => {
                        if !instance.same_as(&w.instance) {
                            // The replaced process was last heard no later than now,
                            // and is not acknowledged from now on. Its leases are not
                            // given up here even when it is proven stopped: the phase,
                            // taken next, may send a switching lease's `Start` to the
                            // new process, and the first heartbeat decides the rest.
                            if predecessor_stopped {
                                w.stopped_process = Some(w.process);
                            }
                            w.process += 1;
                            // Not before a hold made for the leases taken in for it
                            // ([`Scheduler::hold_recovered_leases`]).
                            let ends = w.last_heard.saturating_add(HANDOVER_GRACE);
                            w.handover_ends = w.handover_ends.max(ends);
                        }
                        w.instance = instance;
                        w.capacity = capacity;
                        w.raw_memory_bytes = raw_memory_bytes;
                        w.caps = caps;
                        w.last_heard = now;
                        w.session += 1;
                        // The readings of the session before say nothing about this one:
                        // its `seq` starts again.
                        w.load_state.new_session();
                    }
                    None => {
                        let mut fresh = Worker {
                            capacity,
                            raw_memory_bytes,
                            caps,
                            booked: Resources::default(),
                            leases: 0,
                            whole: false,
                            devices: BTreeSet::new(),
                            load_state: WorkerLoad::default(),
                            last_heard: now,
                            session: 0,
                            instance,
                            process: 0,
                            handover_ends: FarmTime::default(),
                            stopped_process: None,
                            phase: NodePhase::Serving,
                            switch_ends: FarmTime::default(),
                        };
                        self.adopt(&worker, &mut fresh);
                        self.workers.insert(worker, fresh);
                    }
                }
                Vec::new()
            }
            Event::Capacity {
                worker,
                capacity,
                caps,
            } => {
                let raw_memory_bytes = capacity.memory_bytes;
                let capacity = self.memory_percent.of_capacity(capacity);
                if let Some(w) = self.workers.get_mut(&worker) {
                    w.capacity = capacity;
                    w.raw_memory_bytes = raw_memory_bytes;
                    w.caps = caps;
                    w.last_heard = w.last_heard.max(self.now);
                }
                Vec::new()
            }
            Event::Heartbeat {
                worker,
                running,
                load,
            } => match self.workers.get_mut(&worker) {
                Some(w) => {
                    w.last_heard = w.last_heard.max(self.now);
                    if let Some(load) = load {
                        let adm = &mut self.admission;
                        w.load_state.accept(
                            load,
                            self.now,
                            &w.capacity,
                            w.raw_memory_bytes,
                            &adm.cfg,
                            &mut adm.counters,
                        );
                    }
                    self.reconcile(&worker, &running)
                }
                None => Vec::new(),
            },
            Event::Phase { worker, phase } => self.take_phase(&worker, phase),
            Event::Submit { waiter, request } => self.submit(waiter, request),
            Event::Committed(record) => self.committed(&record),
            Event::Started { operation, lease } => {
                self.started(operation, lease);
                Vec::new()
            }
            Event::Report {
                operation,
                lease,
                outcome,
            } => self.report(operation, lease, outcome),
            Event::Tick => {
                let mut effects = Vec::new();
                effects.extend(self.expire());
                self.place(&mut effects);
                effects
            }
            Event::Cordon { worker } => {
                self.cordons.cordon(worker);
                Vec::new()
            }
            Event::Drain { worker, deadline } => {
                self.cordons.drain(worker, deadline);
                Vec::new()
            }
            Event::Uncordon { worker } => {
                // Work may have waited for this worker alone: place it now, not at the
                // next tick.
                self.cordons.uncordon(&worker);
                let mut effects = Vec::new();
                self.place(&mut effects);
                effects
            }
        };
        self.retire();
        let (now, held) = (self.now, &self.held);
        let ops = &self.ops;
        self.cordons.progress(now, |worker| {
            held.values().any(|h| {
                ops[&h.operation]
                    .state
                    .holding()
                    .is_some_and(|(_, w)| w == worker)
            })
        });
        effects
    }
}
