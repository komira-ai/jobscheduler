//! Commit before release: a record's effects are carried out only after its rows are
//! written.
//!
//! The scheduler asks for a record to be committed, and the core feeds it back at once. The
//! effects that follow (the `Start` of a granted lease, the answer of a result, the
//! refusal of an operation) are held in [`Engine::unwritten`] until the [`Writer`] has
//! taken the rows each settles, oldest first, so a `Start` is never sent for a lease that
//! no row names, and a result is never written before the grant it follows. A writer that
//! cannot be reached holds everything behind the earliest row it refused; a writer that
//! refuses a row for good drops that record's remaining effects.

use std::collections::VecDeque;

use js_types::sched::{Effect, OperationId};

use crate::adapter::Adapter;
use crate::engine::{Engine, finish_of};

/// How a write of the rows an effect settles ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The rows are written.
    Taken,
    /// They could not be: the same write may succeed later, and everything behind it
    /// waits.
    Transient,
    /// They cannot be, and trying again cannot help: the effect and the rest of its
    /// record are dropped.
    Dropped,
}

/// Takes the rows the effects of a committed record settle. A core starts with a writer
/// that takes everything, so a process without a store carries effects out as they commit.
pub trait Writer: Send {
    /// Writes the rows `effect` settles: the grant a `Start` follows, the result an
    /// answer carries, the refusal of an operation. An effect that settles no row is
    /// taken.
    fn write(&mut self, effect: &Effect) -> Attempt;
}

impl<F> Writer for F
where
    F: FnMut(&Effect) -> Attempt + Send,
{
    fn write(&mut self, effect: &Effect) -> Attempt {
        self(effect)
    }
}

impl Engine {
    /// Writes the held-back effects of committed records, oldest first, and returns the
    /// ones the writer took, for the caller to carry out. Stops at the first row the
    /// writer refuses because it cannot be reached: it and everything after it wait for
    /// the next call, so no later row is written ahead of an earlier one. A row refused
    /// for good drops its record's remaining effects.
    pub(crate) fn release_unwritten(&mut self, a: &mut dyn Adapter) -> Vec<Effect> {
        let mut ready = Vec::new();
        loop {
            let Some(front) = self.unwritten.front() else {
                return ready;
            };
            let Some(effect) = front.front().cloned() else {
                self.unwritten.pop_front();
                continue;
            };
            match self.writer.write(&effect) {
                Attempt::Taken => {
                    self.unwritten.front_mut().and_then(VecDeque::pop_front);
                    ready.push(effect);
                }
                Attempt::Transient => {
                    let held: usize = self.unwritten.iter().map(VecDeque::len).sum();
                    tracing::warn!(held, "writer holds back records; retried later");
                    return ready;
                }
                Attempt::Dropped => {
                    self.write_drops += 1;
                    tracing::error!("a committed record's remaining effects were dropped");
                    let dropped = self.unwritten.pop_front().unwrap_or_default();
                    self.forget_details(a, dropped);
                }
            }
        }
    }

    /// Whether the finish of `operation` (its answer or refusal) is held back, unwritten.
    /// Bounded by what is held, which is capped.
    pub(crate) fn finish_held(&self, operation: OperationId) -> bool {
        self.unwritten
            .iter()
            .flatten()
            .any(|effect| finish_of(effect) == Some(operation))
    }

    /// Forgets what is parked for the answers among `dropped` effects, which will never be
    /// written, and the acknowledgement owed for their results: the host keeps a result it
    /// is not acknowledged for. What the writer has taken stays: it is settled after this
    /// feed's writes, whatever a later record does.
    fn forget_details(&mut self, a: &mut dyn Adapter, dropped: VecDeque<Effect>) {
        for effect in dropped {
            if let Effect::Answer(answer) = effect {
                a.forget_parcel(answer.lease);
                // Never written, so never acknowledged: the host resends the result.
                self.owed.remove(&answer.lease);
            }
        }
    }

    /// Forgets what is parked for every report whose answer is no longer held.
    pub(crate) fn forget_parked(&self, a: &mut dyn Adapter) {
        let held = &self.unwritten;
        a.retain_parcels(&|lease| {
            let answers =
                |effect: &Effect| matches!(effect, Effect::Answer(ans) if ans.lease == lease);
            held.iter().flatten().any(answers)
        });
    }
}
