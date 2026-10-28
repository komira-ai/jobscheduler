//! A host's `Result` from the report to the answer: what its lease stands for now, and how
//! the scheduler took it. The lease id is the attempt: a re-grant of an operation gets a
//! new lease id, so a `Result` of an earlier attempt names a lease the core no longer
//! holds.

use js_sched::{Event, OpState};
use js_types::sched::{Answer, FarmTime, LeaseId, OperationId, Outcome, WorkerId};

use crate::adapter::{Adapter, Delivery};
use crate::engine::Engine;

/// Why a `Result` is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The operation is answered already, by this lease or another.
    Duplicate,
    /// The lease was given up or granted again, or another node holds it: it is not the
    /// current attempt.
    Stale,
    /// No lease of this process has this id.
    UnknownLease,
}

/// What a lease stands for as a host's `Result` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// `worker` holds the operation's current lease: the `Result` may be taken.
    Current(OperationId),
    /// The scheduler finished the operation with this lease's result, and the finish
    /// waits to be written: the `Result` is acknowledged when it is.
    Held,
    /// The `Result` is refused, for this reason.
    Refused(Reason),
}

/// How the scheduler took a `Result` that was fed to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// It became the operation's result, or a memory kill or lost run the scheduler took.
    Accepted,
    /// The scheduler did not take it, for this reason.
    Refused(Reason),
    /// The scheduler took it, but its finish is not written yet: acknowledged once it is.
    Held,
    /// The scheduler took it, and the record was dropped unwritten: never acknowledged.
    Dropped,
}

/// A host's report that `lease` of `operation` ended with `outcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The operation, as [`Standing::Current`] named it.
    pub operation: OperationId,
    /// The lease.
    pub lease: LeaseId,
    /// How the run ended.
    pub outcome: Outcome,
}

/// The lease whose result finished an operation in `state`, if it is finished.
const fn finished_lease(state: &OpState) -> Option<LeaseId> {
    match state {
        OpState::Completed { lease, .. } | OpState::Failed { lease, .. } => Some(*lease),
        _ => None,
    }
}

impl Engine {
    /// What `lease` stands for as `worker`'s `Result` names it. Only this process's leases
    /// are in `sent`, so a lease of an earlier process (another term) is unknown; one whose
    /// operation was answered since is remembered ([`Engine::answered`]) for as long as the
    /// scheduler keeps the operation.
    pub(crate) fn standing(&self, lease: LeaseId, worker: &WorkerId) -> Standing {
        let Some(&operation) = self.sent.get(&lease) else {
            return Standing::Refused(if self.answered.contains_key(&lease) {
                Reason::Duplicate
            } else {
                Reason::UnknownLease
            });
        };
        let state = self.sched.state(operation);
        // One body for both states: a guard on the alternatives would be two copies of it.
        let current = match state {
            Some(
                OpState::Leased {
                    lease: held,
                    worker: holder,
                    committed: true,
                }
                | OpState::Running {
                    lease: held,
                    worker: holder,
                },
            ) => *held == lease && holder == worker,
            _ => false,
        };
        if current {
            return Standing::Current(operation);
        }
        // Given up, granted again (to this node or another), held by another node, or
        // finished with another lease's result: not the current attempt.
        if state.and_then(finished_lease) != Some(lease) {
            return Standing::Refused(Reason::Stale);
        }
        if self.finish_held(operation) {
            Standing::Held
        } else {
            Standing::Refused(Reason::Duplicate)
        }
    }

    /// How many runs of `operation` the scheduler has recorded as killed for memory or as
    /// the farm's fault.
    fn runs_recorded(&self, operation: OperationId) -> usize {
        let memory = self.sched.memory_runs(operation).map_or(0, <[_]>::len);
        let farm = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
        memory + farm
    }

    /// Feeds `worker`'s report, and returns the answers it led to and whether the
    /// scheduler took it. The in-process log commits its record at once: an accepted
    /// result is answered here, and a refused one (its lease given up while its outputs
    /// were checked) never is. A memory kill or lost run the scheduler takes is answered,
    /// or records a run and runs the operation again.
    pub(crate) fn take_report(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        report: Report,
    ) -> (Vec<Answer>, Taken) {
        let Report {
            operation,
            lease,
            outcome,
        } = report;
        // Named before the feed: a report that finishes the operation may drop its callers.
        let name = a.name(&self.view(), operation);
        let recorded_before = self.runs_recorded(operation);
        // The time is told first, so a hook that reads the adapter's clock sees this
        // report's, not the previous feed's; the feed tells it again.
        a.clock(now);
        a.before_report(&self.view(), worker, &report);
        let event = Event::Report {
            operation,
            lease,
            outcome,
        };
        let answers = self.feed(a, now, event);
        let rerun = self.runs_recorded(operation) > recorded_before;
        let accepted = rerun || answers.iter().any(|answer| answer.lease == lease);
        // Not answered: refused, or finished by this lease with the finish waiting to be
        // written, or with its record dropped unwritten (a finish neither held nor
        // answered).
        let taken = if accepted {
            Taken::Accepted
        } else {
            match self.standing(lease, worker) {
                Standing::Held => Taken::Held,
                Standing::Refused(Reason::Duplicate) => Taken::Dropped,
                Standing::Refused(reason) => {
                    log_refusal(worker, lease, reason);
                    Taken::Refused(reason)
                }
                Standing::Current(_) => {
                    log_refusal(worker, lease, Reason::Stale);
                    Taken::Refused(Reason::Stale)
                }
            }
        };
        if taken == Taken::Held {
            self.owed.insert(lease, worker.clone());
        }
        let (operation, node) = (name, worker);
        match outcome {
            Outcome::Completed { .. } => {
                tracing::info!(%operation, %lease, %node, accepted, "lease finished ok");
            }
            Outcome::Failed(failure) => {
                tracing::warn!(%operation, %lease, %node, ?failure, accepted, "lease finished failed");
            }
        }
        a.after_report(&self.view(), worker, &report, &answers, taken);
        (answers, taken)
    }

    /// Settles the answer of a finished operation: remembers the lease as answered, forgets
    /// the leases of the operation, and has the adapter work out what its callers get.
    pub(crate) fn settle(&mut self, a: &mut dyn Adapter, answer: &Answer) -> Delivery {
        self.answered.insert(answer.lease, answer.operation);
        self.sent
            .retain(|_, operation| *operation != answer.operation);
        // A result whose finish waited to be written is acknowledged to its sender's
        // newest stream: the rows are taken.
        let ack = self.owed.remove(&answer.lease);
        let delivery = a.settle(&self.view(), answer, ack.as_ref());
        self.forget_dropped(a);
        delivery
    }
}

/// Logs why a `Result` of `lease` from `worker` is refused, one line for each reason.
pub(crate) fn log_refusal(worker: &WorkerId, lease: LeaseId, reason: Reason) {
    match reason {
        Reason::Duplicate => {
            tracing::info!(%worker, %lease, "result is a copy: its operation is answered");
        }
        Reason::Stale => {
            tracing::warn!(%worker, %lease, "result refused: stale attempt, the lease was given up or granted again, or another node holds it");
        }
        Reason::UnknownLease => {
            tracing::warn!(%worker, %lease, "result refused: unknown lease, or one of another epoch");
        }
    }
}
