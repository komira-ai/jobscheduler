//! What a core does when the other frontends of its state store take its work over (a retire
//! or a drain): it stops taking any ([`Core::fence`]), has its host streams ended so the
//! daemons reconnect to another frontend (the adapter's: the core owns no stream), answers
//! the callers it still holds from what the others write to the state store
//! ([`Core::follow`]), and keeps an operator's cordons across the change.
//!
//! What a handover needs of the process around the core, and the core cannot do, is the
//! embedder's: ending its host streams, waiting until no result is half taken, and running
//! [`Core::follow`] every [`FOLLOW_EVERY`] for as long as the process lives.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use js_store::{Progress, StateError};
use js_types::sched::{OperationId, WaiterId};

use crate::adapter::{Adapter, Stage};
use crate::core::Core;
use crate::engine::Engine;

/// How many looks in a row at the state store may find an operation neither unfinished nor
/// ended before it is logged as untraced (a WARN). A writer ends an operation in one
/// transaction (its rows removed, its result put), so a look finds one or the other; a store
/// that a release ending operations in two transactions wrote can hold neither for good, and
/// a result past its retention is also gone. Only a log line: an untraced operation's callers
/// are not answered, they keep waiting ([`Core::follow`]).
const UNTRACED_LOOKS: u32 = 2;

/// How often a fenced core that was handed to a follower should be given to
/// [`Core::follow`], for the results of the calls it kept open to be shown.
pub const FOLLOW_EVERY: Duration = Duration::from_millis(200);

/// How long [`Core::flush_unsettled`]'s caller waits between two writes of what the state
/// store refused.
pub const FLUSH_LOOK: Duration = Duration::from_millis(10);

/// How an attempt to write the requeues the state store refused ended.
pub(crate) enum Flushed {
    /// Every requeue is written.
    Settled,
    /// A newer writer holds the store: these operations' requeues will not be written.
    Superseded(Vec<OperationId>),
    /// The store refused again: these operations' requeues are still unwritten.
    Refused(Vec<OperationId>),
}

impl<A: Adapter> Core<A> {
    /// Whether the state store has refused a write of this core because a newer writer holds
    /// it: nothing it writes can succeed any more.
    #[must_use]
    pub fn superseded(&self) -> bool {
        self.lock().engine.superseded
    }

    /// Writes, to the state store, the requeues it refused (a lease the scheduler gave up
    /// while the store did not take the write), so that the successor recovers each
    /// operation queued and not under the lease this core gave up. Works on a fenced core,
    /// which feeds the scheduler nothing and so would never write them again. Tries again
    /// each time `again` says to, until none is left, and stops at once when the store says a
    /// newer writer holds it (nothing may be written then); whatever is left is logged at
    /// ERROR, naming the operations. Returns the operations whose requeues were not written.
    ///
    /// `again` is the embedder's wait: it is called when the store refused again, waits
    /// [`FLUSH_LOOK`] on the embedder's clock without holding anything of the core, and
    /// returns whether to try again, `false` once the embedder's deadline has passed.
    pub async fn flush_unsettled<F, Fut>(&self, mut again: F) -> Vec<OperationId>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let (left, why) = loop {
            // The lock is not held across the wait.
            let step = {
                let mut locked = self.lock();
                let crate::core::Locked { engine, adapter } = &mut *locked;
                engine.flush_step(adapter)
            };
            match step {
                Flushed::Settled => return Vec::new(),
                Flushed::Superseded(left) => break (left, "the state store has a newer writer"),
                Flushed::Refused(left) => {
                    if !again().await {
                        break (
                            left,
                            "the state store kept refusing them until the deadline",
                        );
                    }
                }
            }
        };
        let operations: Vec<String> = left.iter().map(|op| format!("{:016x}", op.0)).collect();
        tracing::error!(
            count = left.len(),
            operations = %operations.join(","),
            why,
            "requeues not written to the state store: the successor recovers these operations \
             with the lease they held"
        );
        left
    }

    /// Answers, from the state store, the callers this fenced core still holds, and the
    /// clients that asked it for an operation by name ([`Adapter::watched`]): an operation the
    /// successor finished gets its result from the `done/` row, one it has leased is shown
    /// executing. A caller waits until its operation finishes, it gives up, or this process is
    /// killed: no look count or timer ends it. An operation the store has no trace of keeps
    /// its callers waiting; it is logged at WARN once the looks in a row that found no trace
    /// reach a few, so an operator sees it. Call it every [`FOLLOW_EVERY`]; returns how many
    /// callers are still waiting.
    ///
    /// # Errors
    /// The state store cannot be read.
    pub fn follow(&self) -> Result<usize, StateError> {
        let mut locked = self.lock();
        let crate::core::Locked { engine, adapter } = &mut *locked;
        engine.follow(adapter)
    }
}

impl Engine {
    /// Writes the requeues the state store has not taken, and the ones the scheduler has
    /// given up since the last feed, once. Works on a fenced core: this is the one thing a
    /// fenced core writes, because the successor's recovery depends on it
    /// ([`Core::flush_unsettled`]).
    pub(crate) fn flush_step(&mut self, a: &mut dyn Adapter) -> Flushed {
        if !self.superseded {
            self.log_requeues(a);
        }
        let left: Vec<OperationId> = self
            .unsettled
            .iter()
            .map(|unsettled| unsettled.operation())
            .collect();
        match (left.is_empty(), self.superseded) {
            (true, _) => Flushed::Settled,
            (false, true) => Flushed::Superseded(left),
            (false, false) => Flushed::Refused(left),
        }
    }

    /// The callers of `operation` still waiting: not answered, and listening.
    fn callers_waiting(&self, a: &dyn Adapter, operation: OperationId) -> Vec<WaiterId> {
        let callers = self.sched.waiters(operation).unwrap_or_default();
        a.waiting_among(callers)
    }

    /// [`Core::follow`] under the lock: how many callers still wait.
    pub(crate) fn follow(&mut self, a: &mut dyn Adapter) -> Result<usize, StateError> {
        let Some(rows) = self.rows.clone() else {
            return Ok(0);
        };
        let mut looked = Vec::new();
        for operation in self.sched.unfinished() {
            let callers = self.callers_waiting(&*a, operation);
            if !callers.is_empty() {
                looked.push((operation, callers, rows.progress(operation)?));
            }
        }
        // An operation nobody waits on any more (its callers gave up) has nothing to report.
        self.untraced_kept
            .retain(|operation, _| looked.iter().any(|(looked, ..)| looked == operation));
        let mut waiting = 0;
        for (operation, callers, progress) in looked {
            let stage = note(&mut self.untraced_kept, operation, &progress);
            let ended = matches!(stage, Some(Stage::Done(_)));
            waiting += callers.len() * usize::from(!ended);
            if let Some(stage) = stage {
                a.show_callers(&callers, &stage);
            }
        }
        // The operations clients ask for by name that this core does not hold are looked at
        // too: a fenced core takes no intake poll, and answers them from here.
        self.follow_watched(a);
        Ok(waiting)
    }

    /// Shows each watched operation what the store says of it now. A watch nobody holds any
    /// more is dropped by the adapter, and one whose result has been shown is not looked at
    /// again. A look the store fails is made again at the next poll, and an operation the
    /// store has no trace of is shown nothing, so its watchers wait, as the callers of an
    /// operation the store lost do.
    pub(crate) fn follow_watched(&mut self, a: &mut dyn Adapter) {
        for operation in a.watched() {
            let mut looked = None;
            self.attempt("a watched operation", &mut |rows| {
                looked = Some(rows.progress(operation)?);
                Ok(())
            });
            if let Some(stage) = looked.as_ref().and_then(stage_of) {
                a.show_watched(operation, &stage);
            }
        }
    }
}

/// What the callers of an operation at `progress` are shown: `None` leaves them as they are
/// (queued, or no trace: they wait).
fn stage_of(progress: &Progress) -> Option<Stage> {
    match progress {
        Progress::Ended(ended) => Some(Stage::Done(ended.clone())),
        Progress::Running => Some(Stage::Executing),
        Progress::Queued | Progress::Gone => None,
    }
}

/// One look at `operation`, whose callers wait: what they are shown now (`None` leaves them
/// as they are). An operation the store has no trace of is shown nothing: it is counted in
/// `untraced`, logged once it has been untraced for a few looks in a row, and its callers go
/// on waiting. A look that finds it starts the count over.
pub(crate) fn note(
    untraced: &mut BTreeMap<OperationId, u32>,
    operation: OperationId,
    progress: &Progress,
) -> Option<Stage> {
    if progress.is_gone() {
        let looks = untraced.entry(operation).or_default();
        *looks = looks.saturating_add(1);
        if *looks == UNTRACED_LOOKS {
            tracing::warn!(%operation, "no trace in the state store: callers keep waiting");
        }
    } else {
        untraced.remove(&operation);
    }
    stage_of(progress)
}
