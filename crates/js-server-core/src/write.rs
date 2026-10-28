//! Commit before release: a record's effects are carried out only after its rows are
//! written.
//!
//! The scheduler asks for a record to be committed, and the core feeds it back at once. The
//! effects that follow (the `Start` of a granted lease, the answer of a result, the
//! refusal of an operation) are held in [`Engine::unwritten`] until the [`Writer`] has
//! taken the rows each settles, oldest first, so a `Start` is never sent for a lease that
//! no row names, and a result is never written before the grant it follows. A writer that
//! cannot be reached holds everything behind the earliest row it refused; a writer that
//! refuses a row for good drops that record's remaining effects, and one that finds
//! another writer's work in the way also undoes what the core did for the record.

use std::collections::VecDeque;

use js_store::Ending;
use js_types::sched::{Effect, LeaseGrant, OperationId, StartLease};

use crate::adapter::Adapter;
use crate::engine::{Engine, finish_of};

/// How a write of the rows an effect settles ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The rows are written.
    Taken,
    /// They could not be: the store could not be reached, the same write may succeed
    /// later, and everything behind it waits.
    Transient,
    /// A row the write depends on is gone or changed: another writer got there first, and
    /// the write is stale. The effect and the rest of its record are dropped, and what the
    /// core did for the record is undone: a grant is taken back, and the callers of a
    /// result or a refusal are answered from the store. A write the store applied and did
    /// not acknowledge is not this: each write finds its own work done and is taken.
    Stale,
    /// The write is for a node that another frontend owns now. It is dropped as a stale
    /// one is, and trying again cannot help; a grant stays as the core made it, since
    /// nothing in the store says it lost, and the callers of a finish are answered from
    /// the store.
    Disowned,
    /// They cannot be written, and trying again cannot help: the effect and the rest of
    /// its record are dropped.
    Dropped,
}

/// What the core knows of the effect it asks a writer to write, besides the effect.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum Settles<'a> {
    /// The record the effect follows is a grant: the grant as it was committed, with the
    /// cost admission charged. The `Start` of the lease is the effect.
    Grant(&'a LeaseGrant),
    /// The effect finishes an operation (an answer or a refusal): how it ended, as the
    /// adapter says ([`Adapter::ending`](crate::Adapter::ending)).
    Ending(&'a Ending),
    /// The core knows nothing more: a writer of a core that has no store, or an effect the
    /// adapter cannot say how it ended.
    Nothing,
}

/// Takes the rows the effects of a committed record settle. A core starts with a writer
/// that takes everything, so a process without a store carries effects out as they commit;
/// [`Core::open`](crate::Core::open) installs the one that writes the state store.
pub trait Writer: Send {
    /// Writes the rows `effect` settles: the grant a `Start` follows, the result an
    /// answer carries, the refusal of an operation. An effect that settles no row is
    /// taken.
    fn write(&mut self, effect: &Effect, settles: &Settles<'_>) -> Attempt;
}

impl<F> Writer for F
where
    F: FnMut(&Effect, &Settles<'_>) -> Attempt + Send,
{
    fn write(&mut self, effect: &Effect, settles: &Settles<'_>) -> Attempt {
        self(effect, settles)
    }
}

/// The effects of one committed record, which leave together or not at all.
#[derive(Debug)]
pub(crate) struct Batch {
    pub(crate) effects: VecDeque<Effect>,
    /// The grant of the record, if it was a grant: what the `Start` among the effects
    /// writes.
    pub(crate) grant: Option<LeaseGrant>,
}

impl Engine {
    /// Writes the held-back effects of committed records, oldest first, and returns the
    /// ones the writer took, for the caller to carry out. Stops at the first row the
    /// writer refuses because it cannot be reached: it and everything after it wait for
    /// the next call, so no later row is written ahead of an earlier one. A row refused
    /// for good drops its record's remaining effects; one refused as stale also gives up
    /// what the core did for the record ([`Engine::lost_race`]).
    pub(crate) fn release_unwritten(&mut self, a: &mut dyn Adapter) -> Vec<Effect> {
        let mut ready = Vec::new();
        loop {
            let Some(front) = self.unwritten.front() else {
                return ready;
            };
            let Some(effect) = front.effects.front().cloned() else {
                self.unwritten.pop_front();
                continue;
            };
            let grant = front.grant.clone();
            // Only a store has rows to describe: a plain writer is never asked how the
            // operation ended.
            let ending = self
                .rows
                .as_ref()
                .and_then(|_| a.ending(&effect, self.finished_retention));
            let settles = match (&grant, &ending) {
                (Some(grant), _) => Settles::Grant(grant),
                (None, Some(ending)) => Settles::Ending(ending),
                (None, None) => Settles::Nothing,
            };
            let attempt = self.writer.write(&effect, &settles);
            if attempt == Attempt::Transient {
                self.unreachable = true;
            }
            match attempt {
                Attempt::Taken => {
                    self.unwritten
                        .front_mut()
                        .and_then(|batch| batch.effects.pop_front());
                    ready.push(effect);
                }
                Attempt::Transient => {
                    let held: usize = self.unwritten.iter().map(|b| b.effects.len()).sum();
                    tracing::warn!(held, "writer holds back records; retried later");
                    return ready;
                }
                Attempt::Stale | Attempt::Disowned | Attempt::Dropped => {
                    self.write_drops += 1;
                    tracing::error!("a committed record's remaining effects were dropped");
                    let dropped = self.unwritten.pop_front().map(|b| b.effects);
                    let dropped = dropped.unwrap_or_default();
                    // Another frontend's work stands in a stale write's place: this core
                    // gives up what it did for the operation and follows the store. A
                    // finish dropped for another reason (the node moved to another
                    // frontend, whose daemon keeps the result) is followed too, but its
                    // grants stay: nothing in the store says they lost.
                    if attempt == Attempt::Stale {
                        self.lost_race(a, &dropped);
                    } else {
                        self.follow_finishes(a, &dropped);
                    }
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
            .flat_map(|batch| &batch.effects)
            .any(|effect| finish_of(effect) == Some(operation))
    }

    /// Whether the scheduler holds `operation`, or finished it and holds the finish back,
    /// unwritten. The store's rows of such an operation are no news: taking them in would
    /// run the operation again.
    pub(crate) fn known_operation(&self, operation: OperationId) -> bool {
        self.sched.state(operation).is_some() || self.finish_held(operation)
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
            held.iter().flat_map(|batch| &batch.effects).any(answers)
        });
    }

    /// Undoes what this core did for the operations of the effects the store refused as
    /// stale: another frontend's work stands in their place. A grant is undone, so the
    /// worker's booking is released and the scheduler does not keep a lease the store does
    /// not hold; the callers of a result or a refusal that lost are answered from the store.
    fn lost_race(&mut self, a: &mut dyn Adapter, dropped: &VecDeque<Effect>) {
        for effect in dropped {
            if let Effect::Start(start) = effect {
                self.lose_grant(a, start);
            }
        }
        self.follow_finishes(a, dropped);
    }

    /// Hands the callers of the results and refusals among the `dropped` effects to the
    /// adapter, to be answered from the store: a finish that was not written (the daemon
    /// moved to another frontend, or another frontend finished the operation) is the
    /// store's to tell.
    fn follow_finishes(&mut self, a: &mut dyn Adapter, dropped: &VecDeque<Effect>) {
        for effect in dropped {
            match effect {
                Effect::Answer(answer) => a.followed(answer.operation, &answer.waiters),
                Effect::Refuse(refusal) => a.followed(refusal.operation, &refusal.waiters),
                Effect::Start(_) | Effect::Commit(_) | Effect::Waiting(_) => {}
            }
        }
    }

    /// The grant `start` lost to another frontend: the scheduler forgets the placement and
    /// the store says where the operation stands.
    fn lose_grant(&mut self, a: &mut dyn Adapter, start: &StartLease) {
        let unplaced = self.sched.unplace(start.lease);
        unplaced
            .into_iter()
            .for_each(|operation| self.undone(a, operation, start));
    }

    /// `operation`, whose grant `start` was undone, is forgotten if the store has no queue
    /// row for it: another frontend leased or finished it, and its callers follow the
    /// store. A row that cannot be read leaves it queued, and a requeue the store has not
    /// taken yet is why the row is not there: the operation stays.
    fn undone(&mut self, a: &mut dyn Adapter, operation: OperationId, start: &StartLease) {
        tracing::info!(
            %operation,
            lease = %start.lease,
            node = %start.worker,
            "another frontend took the operation: the placement is undone"
        );
        let mut looked = None;
        self.attempt("a queue row", &mut |rows| {
            looked = Some(rows.queue_version(operation)?);
            Ok(())
        });
        if looked == Some(None) && !self.requeue_pending(operation) {
            let waiters = self.sched.forget_queued(operation).unwrap_or_default();
            a.followed(operation, &waiters);
        }
    }
}
