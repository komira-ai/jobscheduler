//! The worker's half of a whole-node lease: the answers it keeps, tagged with their lease,
//! and [`WorkerHost`], the session's non-blocking way to ask for them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use super::{Handle, Order};
use crate::exclusive::Quiesce;
use crate::lifecycle::{Event, Lifecycle, Quiet, Restored};
use crate::readiness::WorkerHook;

/// What the worker found about a whole-node lease, each answer tagged with the lease it is
/// about. The worker keeps the last of each. Public (and hidden) only so a test can drive
/// the rules over a lifecycle of its own.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct LeaseAnswers {
    quiet: Option<(String, Quiet)>,
    restored: Option<(String, Restored)>,
}

impl LeaseAnswers {
    /// Whether the host is quiet for `lease`. An answer that is final (not busy) for the
    /// lease is kept, so the hook runs once for a lease however often it is asked.
    pub fn prepare(
        &mut self,
        life: &mut Lifecycle,
        hook: Option<&WorkerHook>,
        lease: String,
        events: &mut Vec<Event>,
    ) {
        let settled = self
            .quiet
            .as_ref()
            .is_some_and(|(held, quiet)| *held == lease && *quiet != Quiet::Busy);
        if settled {
            return;
        }
        let found = life.prepare(&lease, events);
        let answer = match hook {
            Some(hook) => hook.vet(found),
            None => found,
        };
        self.quiet = Some((lease, answer));
    }

    /// Whether the host is clean after `lease`. A lifecycle that still follows a lease is
    /// not: the reply that ended it has not been applied.
    pub fn restore(&mut self, life: &mut Lifecycle, lease: String, events: &mut Vec<Event>) {
        // A second order after `Done` only runs the checks once more, and a finding taints
        // the host as after any job, so it is not skipped.
        let answer = match life.exclusive() {
            Some(_) => Restored::Busy,
            None => life.restore(events),
        };
        self.restored = Some((lease, answer));
    }

    /// The last quiet answer and the lease it is about.
    pub fn quiet(&self) -> Option<&(String, Quiet)> {
        self.quiet.as_ref()
    }

    /// The last restored answer and the lease it is about.
    pub fn restored(&self) -> Option<&(String, Restored)> {
        self.restored.as_ref()
    }

    /// Drops what the lifecycle's lease (`held`) makes stale: a quiet answer about another
    /// lease, and every restored answer once a lease is followed again.
    pub fn follow(&mut self, held: Option<&str>) {
        let stale = match &self.quiet {
            Some((lease, _)) => Some(lease.as_str()) != held,
            None => false,
        };
        if stale {
            self.quiet = None;
        }
        if held.is_some() {
            self.restored = None;
        }
    }
}

/// The session's way to ask the worker about a whole-node lease: a [`Quiesce`] host that
/// never blocks. Each question that the worker has not finally answered sends an order
/// without waiting (a full queue only means the question is asked again on the next
/// heartbeat) and the answer is what the worker last published, if that is about the lease
/// asked. Any other answer, or none yet, is busy, so an answer for one lease is never taken
/// for another. A final answer is not asked for twice, and at most one question of each
/// kind waits in the queue, so questions never crowd heartbeat replies out of it. The order
/// names no lease: the host records the lease of each question and the worker answers for
/// the latest one, so a queued question is never about a lease that has been asked past.
pub struct WorkerHost {
    handle: Handle,
}

impl WorkerHost {
    /// A host that asks the worker `handle` belongs to.
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// Records `lease` as the latest asked, then sends `order` (which names no lease)
    /// unless a question of its kind already waits in the queue: the worker clears `queued`
    /// when it takes the order and then answers for the latest lease recorded, so at most
    /// one of each kind is queued, it is about the lease asked last, and questions never
    /// crowd replies out. A queue with no room drops the order.
    fn ask(
        &self,
        order: Order,
        lease: &str,
        (asked, queued): (&Mutex<Option<String>>, &AtomicBool),
    ) {
        *asked.lock().unwrap_or_else(PoisonError::into_inner) = Some(lease.to_owned());
        if queued.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.handle.orders.try_send(order).is_err() {
            queued.store(false, Ordering::SeqCst);
        }
    }
}

impl Quiesce for WorkerHost {
    fn prepare(&mut self, lease: &str) -> Quiet {
        let published = self.handle.shared.lock().answers.quiet.clone();
        match published {
            Some((held, quiet)) if held == lease && quiet != Quiet::Busy => quiet,
            _ => {
                let shared = &self.handle.shared;
                let kind = (&shared.asked_prepare, &shared.prepare_queued);
                self.ask(Order::Prepare, lease, kind);
                Quiet::Busy
            }
        }
    }

    fn restore(&mut self, lease: &str) -> Restored {
        let published = self.handle.shared.lock().answers.restored.clone();
        match published {
            Some((held, Restored::Done)) if held == lease => Restored::Done,
            _ => {
                let shared = &self.handle.shared;
                let kind = (&shared.asked_restore, &shared.restore_queued);
                self.ask(Order::Restore, lease, kind);
                Restored::Busy
            }
        }
    }
}

/// The lease the latest question named, taken so the next order finds only a newer one.
pub(super) fn take_asked(asked: &Mutex<Option<String>>) -> Option<String> {
    asked.lock().unwrap_or_else(PoisonError::into_inner).take()
}
