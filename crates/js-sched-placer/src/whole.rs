//! The scheduler rounds the whole-host jobs of the engine need ([`super::whole_jobs`]).
//!
//! The grants of whole-host jobs are numbered in their own term ([`WHOLE_TERM`]), at a round
//! of their own before any other is placed ([`Engine::whole_tick`]): a round that places a
//! job numbers its lease as the job table does, and a whole-host job granted there instead
//! is put back ([`Engine::settle_whole`]). The helpers here walk the queue's operations
//! (the grants put back, the grants a heartbeat lists) and take the pool's whole-node
//! settings into the scheduler ([`Engine::take_settings`]).

use js_registry::WholeNodeConfig;
use js_sched::{OpState, Scheduler};
use js_types::sched::{FarmTime, LeaseId as SchedLeaseId, OperationId, WorkerId};

use super::{Engine, TERM};

/// The term the engine numbers the grants of the whole-host jobs it queues in.
pub const WHOLE_TERM: u64 = 3;

impl Engine {
    /// Gives the scheduler `settings`, the pool's whole-node settings, when they differ from
    /// the ones it has.
    pub(super) fn take_settings(&mut self, settings: &WholeNodeConfig) {
        if self.whole_settings.as_ref() == Some(settings) {
            return;
        }
        let sched = std::mem::replace(&mut self.sched, Scheduler::new(TERM));
        self.sched = sched.with_whole_node(settings.clone());
        self.whole_settings = Some(settings.clone());
    }

    /// Runs one placement round of the queued whole-host jobs, in a term of their own, so
    /// that their grants are numbered in [`WHOLE_TERM`] and the other terms' sequences are
    /// left as they were. With no whole-host job queued it does nothing.
    pub(super) fn whole_tick(&mut self, now: FarmTime) {
        if self.whole_jobs.is_empty() {
            return;
        }
        let (operation, seq) = (self.sched.next_operation(), self.sched.next_seq());
        self.sched
            .recover(WHOLE_TERM, operation, self.whole_seq, Vec::new());
        self.feed(now, js_sched::Event::Tick);
        self.whole_seq = self.sched.next_seq();
        self.sched.recover(TERM, operation, seq, Vec::new());
    }

    /// Puts back a grant of a whole-host job that a round outside [`Engine::whole_tick`]
    /// made: it is numbered in another term, and the job queues for its own round again.
    pub(super) fn settle_whole(&mut self) {
        let stray: Vec<SchedLeaseId> = self
            .whole_operations()
            .filter_map(|operation| self.pending(operation))
            .filter(|(lease, _)| lease.term != WHOLE_TERM)
            .map(|(lease, _)| lease)
            .collect();
        for lease in stray {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// Puts back every pending grant of a whole-host job on a worker `off` names, keeping the
    /// job's place in the queue.
    pub(super) fn unplace_whole_where(&mut self, off: impl Fn(&WorkerId) -> bool) {
        let on: Vec<SchedLeaseId> = self
            .whole_operations()
            .filter_map(|operation| self.pending(operation))
            .filter(|(_, worker)| off(worker))
            .map(|(lease, _)| lease)
            .collect();
        for lease in on {
            let _ = self.sched.unplace_keeping_queued_at(lease);
        }
    }

    /// The operation of every whole-host job the engine queues.
    pub(super) fn whole_operations(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.whole_jobs.values().copied()
    }

    /// The pending grant of `operation`: its lease and worker, or `None` when it holds none.
    pub(super) fn pending(&self, operation: OperationId) -> Option<(SchedLeaseId, WorkerId)> {
        match self.sched.state(operation) {
            Some(OpState::Leased { lease, worker, .. }) => Some((*lease, worker.clone())),
            _ => None,
        }
    }

    /// The pending grants of the whole-host jobs on `worker`.
    pub(super) fn pending_on(&self, worker: &WorkerId) -> Vec<SchedLeaseId> {
        let pending = self.whole_operations().filter_map(|op| self.pending(op));
        let here = pending.filter(|(_, on)| on == worker);
        here.map(|(lease, _)| lease).collect()
    }
}
