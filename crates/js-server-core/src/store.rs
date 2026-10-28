//! The core over a state store: what it writes, when it learns the store is gone and back,
//! and how a start takes up what the store holds.
//!
//! A core opened over a store ([`Core::open`](crate::Core::open)) writes every change of the
//! scheduler through [`js_store::Rows`] before it acts on it: a submitted request, a lease
//! granted, a result taken, an operation refused (the rows of a committed record, written by
//! the [`Writer`] before its effects are carried out), a lease given up
//! ([`Engine::settle_requeues`]), a daemon's session and the claim of its node. A write the
//! store refuses is told apart by [`classify`]: a store that cannot be reached is an outage
//! the scheduler is told about ([`Engine::watch_store`]), and a write that found another
//! writer's work in its way is dropped.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_sched::{Event, FloorChange, Input, PLACEMENT_ROUND, Recovered, Request, Requeue};
use js_sched::{Fate, RequeueReason};
use js_state::{Store, StoreError};
use js_store::{
    EpochPolicy, Ext, RowCodec, Rows, StateError, StoredCordon, StoredOperation, Submitted,
};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, OperationId, StateMachine, WaiterId, WorkerId,
};

use crate::adapter::Adapter;
use crate::core::{Held, NodeAction, NodeUnread, NotQueued, PlaceError, UnknownNode};
use crate::engine::Engine;
use crate::write::{Attempt, Settles, Writer};

/// How many lease sequences a core wants left in its block before it reserves the next:
/// two placement rounds, so a feed never finds the block spent in the middle of a round of
/// grants.
const SEQS_LOW: u64 = 2 * PLACEMENT_ROUND as u64;

// A fresh block must hold more sequences than SEQS_LOW, or every feed would reserve one.
const _: () = assert!(SEQS_LOW < js_store::ID_BLOCK);

/// The state store a core keeps its rows in, and the encoding of the rows' values.
pub struct Storage {
    store: Arc<dyn Store>,
    policy: EpochPolicy,
    codec: Arc<dyn RowCodec>,
    ext: Arc<dyn Ext>,
}

impl Storage {
    /// The store `store`, whose `op/`, `lease/` and `session/` values are `codec`'s and
    /// whose embedder's rows are `ext`'s. A start keeps the epoch the store holds
    /// ([`Storage::with_epoch`]).
    #[must_use]
    pub fn new(store: Arc<dyn Store>, codec: Arc<dyn RowCodec>, ext: Arc<dyn Ext>) -> Self {
        Self {
            store,
            policy: EpochPolicy::Keep,
            codec,
            ext,
        }
    }

    /// This storage, with `policy` deciding what a start does with the store's epoch.
    #[must_use]
    pub const fn with_epoch(mut self, policy: EpochPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Writes the rows the effects of a committed record settle.
pub(crate) struct StoreWriter(Arc<Rows>);

impl StoreWriter {
    pub(crate) const fn new(rows: Arc<Rows>) -> Self {
        Self(rows)
    }
}

impl Writer for StoreWriter {
    fn write(&mut self, effect: &Effect, settles: &Settles<'_>) -> Attempt {
        let rows = &self.0;
        let written = match (effect, settles) {
            (Effect::Start(_), Settles::Grant(grant)) => rows.granted(grant),
            (Effect::Answer(answer), Settles::Ending(ending)) => {
                rows.finished(answer.operation, answer.lease, ending)
            }
            (Effect::Refuse(refusal), Settles::Ending(ending)) => {
                rows.refused(refusal.operation, ending)
            }
            // The effects of a committed record are a `Start`, an answer or a refusal; one
            // the adapter cannot say how it ended has no row to write, and no retry helps.
            _ => {
                tracing::error!("the adapter did not say how the effect ended: no row to write");
                return Attempt::Dropped;
            }
        };
        match written {
            Ok(()) => Attempt::Taken,
            Err(error) => {
                tracing::error!(%error, "the state store did not take a committed record");
                classify(&error)
            }
        }
    }
}

/// How the write that failed with `error` ended. Only a store that cannot be reached is an
/// outage; a write that lost a compare-and-set or found its row gone is the store working;
/// a write for a node another frontend owns is dropped, and so is one that cannot be encoded
/// or reads a row that is not the scheduler's; a store that says a newer writer holds it
/// supersedes this core.
pub(crate) const fn classify(error: &StateError) -> Attempt {
    match error {
        StateError::Store(StoreError::Failed(_)) => Attempt::Transient,
        StateError::Missing(_) | StateError::Conflict(_) => Attempt::Stale,
        StateError::StaleOwner(_) => Attempt::Disowned,
        StateError::Store(StoreError::Fenced) => Attempt::Superseded,
        StateError::Corrupt { .. } | StateError::Codec(_) => Attempt::Dropped,
    }
}

/// A lease given up that the state store has not been told of.
#[derive(Debug)]
pub(crate) struct Unsettled {
    requeue: Requeue,
    losses: Option<u32>,
    fate: Option<Fate>,
    floors: FloorChange,
}

impl Unsettled {
    /// The operation the lease belongs to.
    pub(crate) const fn operation(&self) -> OperationId {
        self.requeue.operation
    }
}

impl NodeAction {
    /// How the cordon row keeps this action, `at` milliseconds on the state store's clock:
    /// `None` when the node returns to placement.
    fn stored(self, at: u64) -> Option<StoredCordon> {
        match self {
            Self::Cordon => Some(StoredCordon::Cordoned),
            Self::Drain(within) => Some(StoredCordon::Draining {
                deadline_unix_ms: at.saturating_add(millis(within)),
            }),
            Self::Uncordon => None,
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The scheduler input that places a node as its `cordon/` row says, at farm time `now`
/// when the store's clock reads `at` milliseconds: a drain ends as far from `now` as its
/// deadline is from `at`.
pub(crate) fn cordon_event(
    worker: WorkerId,
    cordon: StoredCordon,
    now: FarmTime,
    at: u64,
) -> Event {
    match cordon {
        StoredCordon::Cordoned => Event::Cordon { worker },
        StoredCordon::Draining { deadline_unix_ms } => Event::Drain {
            worker,
            deadline: now
                .saturating_add(Duration::from_millis(deadline_unix_ms.saturating_sub(at))),
        },
    }
}

impl Engine {
    /// Opens `storage`, reads what it holds and takes it up: the core's rows from now on.
    /// `first` is the process term, which names this frontend in the store's `owner/` rows
    /// and is the epoch of a store that has none; the scheduler's term is the epoch.
    ///
    /// The operations that wait for a lease are taken up, and none that holds one: the
    /// lease belongs to the frontend its node is connected to, which may be another than
    /// this, and a lease taken up here would be given up after the lease grace and its
    /// operation run twice. A leased operation is taken up when its node says `Hello` here
    /// ([`Engine::claim_worker`]); if the node never does, the orphan sweep requeues it
    /// ([`Engine::take_intake`]). Each operation taken up is held for a caller of its own
    /// ([`Adapter::adopted`]). The first blocks of operation ids and lease sequences are
    /// reserved past everything any frontend of the store reserved, so a start never takes
    /// an id an earlier one took.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds a row that is not one the scheduler
    /// wrote.
    pub(crate) fn keep_in(
        &mut self,
        a: &mut dyn Adapter,
        first: u64,
        storage: Storage,
        now: FarmTime,
    ) -> Result<(), StateError> {
        let Storage {
            store,
            policy,
            codec,
            ext,
        } = storage;
        let (rows, epoch) = Rows::open(store, policy, first, codec, ext)?;
        // The learned sizes are the adapter's: their rows are neither read nor allowed to
        // fail a start.
        rows.set_read_size_keys(false);
        let stored = rows.load()?;
        // The scheduler's grace for a recovered lease runs from the last time it was told.
        let none = self.sched.apply(Input::new(now, Event::Tick));
        debug_assert!(none.is_empty(), "a tick of an empty scheduler: {none:?}");
        let recovered: Vec<Recovered> = stored
            .operations
            .into_iter()
            .filter(|op| op.lease.is_none())
            .map(|op| self.take_stored(a, op))
            .collect();
        let first_operation = rows.reserve_operation(OperationId(stored.next_operation))?;
        let seqs = rows.reserve_seqs(stored.next_seq)?;
        self.sched.restore_floors(stored.floors);
        self.sched
            .recover(epoch, first_operation, seqs.start, recovered);
        self.sched.use_seq_block(seqs);
        let at = rows.store_ms();
        for (worker, cordon) in stored.cordons {
            self.intake.cordons.insert(worker.clone(), cordon);
            let none = self
                .sched
                .apply(Input::new(now, cordon_event(worker, cordon, now, at)));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        }
        let rows = Arc::new(rows);
        self.writer = Box::new(StoreWriter::new(Arc::clone(&rows)));
        self.rows = Some(rows);
        self.epoch = Some(epoch);
        Ok(())
    }

    /// `op`, an operation the store holds, as the scheduler takes it up: held for a caller
    /// of its own that no client waits on, which the next request for the work joins.
    fn take_stored(&mut self, a: &mut dyn Adapter, op: StoredOperation) -> Recovered {
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        a.adopted(op.operation, waiter, op.parcel.as_deref());
        if let Some(grant) = &op.lease {
            // A recovered operation that holds a lease has been started: if it is queued
            // again, its streams read `WaitingKept`, and its node's `Result` names a lease
            // this core knows.
            self.ever_started.insert(op.operation);
            self.sent.insert(grant.lease, op.operation);
        }
        op.recovered(waiter)
    }

    /// Takes `op`, which the store holds and the scheduler neither holds nor has finished
    /// (the caller checked), into the scheduler.
    pub(crate) fn take_in_stored(&mut self, a: &mut dyn Adapter, op: StoredOperation) {
        let recovered = self.take_stored(a, op);
        // The operation is neither in the scheduler nor finished, so `take_in` has nothing
        // to refuse.
        let taken = self.sched.take_in(recovered);
        debug_assert!(taken, "the stored operation was already in the scheduler");
    }

    /// Runs `write` on the state store, if the core has one, and says how it ended; a
    /// refusal is logged here, and the caller does not act on what depends on it. A store
    /// that could not be reached is remembered ([`Engine::watch_store`] tells the
    /// scheduler). Not generic, so every caller's runs count toward one function.
    pub(crate) fn attempt(
        &mut self,
        what: &str,
        write: &mut dyn FnMut(&Rows) -> Result<(), StateError>,
    ) -> Attempt {
        let Some(rows) = self.rows.clone() else {
            return Attempt::Taken;
        };
        let Err(error) = write(&rows) else {
            return Attempt::Taken;
        };
        tracing::error!(%error, "the state store did not take {what}");
        let attempt = classify(&error);
        match attempt {
            Attempt::Transient => self.unreachable = true,
            Attempt::Superseded => self.superseded = true,
            _ => {}
        }
        attempt
    }

    /// Tells the scheduler when the state store went away and when it came back (see
    /// [`js_sched::Scheduler::store_unavailable`]). A write that could not reach it raises
    /// the flag; while it is raised each feed tries one write, and the first that takes ends
    /// the outage. Called at the start of every feed, so no lease is judged expired by a
    /// tick that comes before the core knows the store is back.
    pub(crate) fn watch_store(&mut self, now: FarmTime) {
        let Some(rows) = self.rows.clone() else {
            return;
        };
        if self.superseded {
            // The store is reachable but this core may not write it, so it stays unavailable
            // (no grant, no lease given up) and never pings back.
            if !self.store_down {
                tracing::error!("the state store has a newer writer: this scheduler takes no work");
                self.store_down = true;
                self.sched.store_unavailable();
            }
            return;
        }
        if self.unreachable && !self.store_down {
            tracing::warn!("the state store is unavailable: no lease is given up or granted");
            self.store_down = true;
            self.sched.store_unavailable();
        }
        if self.store_down && rows.ping().is_ok() {
            tracing::info!("the state store is back: expiry waits for the lease grace");
            self.unreachable = false;
            self.store_down = false;
            self.sched.store_available(now);
        }
    }

    /// Reserves the next block of lease sequences once the scheduler has fewer than
    /// [`SEQS_LOW`] left in its block, and gives it to the scheduler, which places no lease
    /// past the end of its block. Called at the start of every feed, after
    /// [`Engine::watch_store`]: while the store is down nothing is placed, and the
    /// reservation waits for it. A reservation the store refuses is tried again at the next
    /// feed, and the scheduler places what is left of its block meanwhile.
    pub(crate) fn reserve_seqs(&mut self) {
        if self.rows.is_none() || self.store_down || self.sched.seqs_left() >= SEQS_LOW {
            return;
        }
        let own = self.sched.next_seq();
        let mut block = None;
        self.attempt("a block of lease sequences", &mut |rows| {
            block = Some(rows.reserve_seqs(own)?);
            Ok(())
        });
        if let Some(block) = block {
            self.sched.use_seq_block(block);
        }
    }

    /// Writes the leases the scheduler has just given up, after the ones the store has not
    /// taken, oldest first, and keeps the ones it still refuses for the next feed. A grant
    /// needs the operation's queue row, which only its requeue writes, so a requeue the
    /// store missed is made up before the grant that follows it, or that grant is refused
    /// and the operation never runs. A grant the store has not taken for a lease the
    /// scheduler has given up is never written: it would put the lease back in the store,
    /// held by no one, over the queue row the requeue writes.
    pub(crate) fn settle_requeues(&mut self, given_up: &[Requeue]) {
        for requeue in given_up {
            for batch in &mut self.unwritten {
                batch
                    .effects
                    .retain(|effect| crate::engine::started(effect) != Some(requeue.lease));
            }
            if self.rows.is_some() {
                let unsettled = self.unsettled_of(requeue);
                self.unsettled.push(unsettled);
            }
        }
        let pending = std::mem::take(&mut self.unsettled);
        for unsettled in pending {
            let Unsettled {
                requeue,
                losses,
                fate,
                floors,
            } = &unsettled;
            let attempt = self.attempt("a requeue", &mut |rows| {
                rows.requeued(requeue, *losses, fate.as_ref(), floors, None)
            });
            // A give-up for a node another frontend owns now is not written again: the
            // node's lease is that frontend's to give up, and the check would refuse it
            // every time.
            self.write_drops += u64::from(attempt == Attempt::Disowned);
            if !matches!(attempt, Attempt::Taken | Attempt::Disowned) {
                self.unsettled.push(unsettled);
            }
        }
    }

    /// What the store is told of the lease `requeue` gives up. Taken now, not when the
    /// store takes the write: a later kill must not reach an earlier requeue's row.
    fn unsettled_of(&mut self, requeue: &Requeue) -> Unsettled {
        let losses = if requeue.reason.is_loss() {
            self.sched.losses(requeue.operation)
        } else {
            None
        };
        // Only a kill past the action's own limit raises a floor, and only a raise forgets
        // one.
        let floors = if matches!(requeue.reason, RequeueReason::OutOfMemory { .. }) {
            self.sched.take_floor_change(requeue.operation)
        } else {
            FloorChange::default()
        };
        Unsettled {
            requeue: requeue.clone(),
            losses,
            fate: self.sched.fate(requeue.operation),
            floors,
        }
    }

    /// Whether a lease of `operation` the scheduler gave up is not yet written to the store
    /// as a requeue: the operation has no `queue/` row for that reason.
    pub(crate) fn requeue_pending(&self, operation: OperationId) -> bool {
        self.unsettled
            .iter()
            .any(|unsettled| unsettled.requeue.operation == operation)
    }

    /// Whether the refusal that `record` proposes is taken back because a daemon connected
    /// to another frontend of the state store could run the operation. The scheduler judged
    /// the operation unservable from its own daemons; the session rows of the store hold
    /// every frontend's, so they decide whether the operation is refused. An operation held
    /// waits again for the unservable wait, and the rows are read again then. A store whose
    /// session rows cannot be read holds it too: a refusal is never final on a guess.
    pub(crate) fn held_for_other_frontends(&mut self, record: &ControlRecord) -> bool {
        let ControlRecord::Refusal(refusal) = record else {
            return false;
        };
        let Some(rows) = self.rows.clone() else {
            return false;
        };
        self.sched.hold_refusal(refusal.operation, |request| {
            rows.session_could_run(request).unwrap_or_else(|error| {
                tracing::warn!(%error, "the daemon sessions cannot be read: the refusal waits");
                true
            })
        })
    }

    /// Writes `request`, about to be queued, to the state store with `parcel`, unless it
    /// will join a twin the scheduler holds, and says where it is held. A twin the
    /// store holds that the scheduler does not is taken up as an operation of its own,
    /// queued or leased on a node this frontend owns (`now` is farm time, for the look at
    /// the owner); one leased on a node another frontend owns is not: that frontend's lease
    /// is not this scheduler's to give up, and the request follows the operation through
    /// the store.
    ///
    /// # Errors
    /// The store did not take it, or the operation's previous run is finishing: the request
    /// is not queued, and the client, who is told so, asks again.
    pub(crate) fn queue(
        &mut self,
        a: &mut dyn Adapter,
        request: &Request,
        parcel: &[u8],
        now: FarmTime,
    ) -> Result<Held, NotQueued> {
        if self.rows.is_none() || self.sched.twin(request).is_some() {
            return Ok(Held::Here);
        }
        let wanted = self.sched.next_operation();
        let mut submitted = None;
        let attempt = self.attempt("a submission", &mut |rows| {
            // The id comes from the block this frontend reserved: the operation's row is
            // put under an id no other frontend takes.
            let id = rows.reserve_operation(wanted)?;
            submitted = Some(rows.submit(id, request, parcel, None)?);
            Ok(())
        });
        if attempt != Attempt::Taken {
            return Err(if self.superseded {
                NotQueued::Superseded
            } else {
                NotQueued::Unavailable
            });
        }
        if let Some(Submitted::Queued(used)) = &submitted {
            self.sched.skip_to(*used);
        }
        let Some(Submitted::Joined(op)) = submitted else {
            return Ok(Held::Here);
        };
        // The store names an operation the scheduler holds and has no twin for: it finished
        // it, and the finish (which removes the rows) is held back. The request cannot join
        // a finished operation, and taking the rows up again would run the work twice. It
        // asks again: once the finish is written, the rows are gone and it queues afresh.
        //
        // The scheduler retires a finished operation after its retention whether or not the
        // finish was written, so the held finish itself is asked too.
        if self.known_operation(op.operation) {
            return Err(NotQueued::Finishing);
        }
        let leased_elsewhere = match &op.lease {
            Some(grant) => !self.owns_worker(now, &grant.worker),
            None => false,
        };
        if leased_elsewhere {
            return Ok(Held::Elsewhere(op.operation));
        }
        self.take_in_stored(a, *op);
        Ok(Held::Here)
    }

    /// Whether the state store knows `worker`: a frontend has owned it. An operator may
    /// place a node connected to another frontend through this one. `None` when the store
    /// cannot say. A core with no state store knows no node but its own.
    fn known_elsewhere(&mut self, worker: &WorkerId) -> Option<bool> {
        if self.rows.is_none() {
            return Some(false);
        }
        let mut known = None;
        self.attempt("a node look", &mut |rows| {
            known = Some(rows.node_known(worker)?);
            Ok(())
        });
        known
    }

    /// Cordons, drains or uncordons `worker` (see `js_sched::Cordon`), after the store has
    /// taken the node's `cordon/` row: a node connected to another frontend of the store is
    /// placed too, and that frontend applies the row. A drain's deadline is written on the
    /// store's clock, the one every frontend shares.
    ///
    /// # Errors
    /// `worker` has never registered with this core, and no frontend of the store has
    /// claimed it; the store cannot say whether one has, and `worker` is not connected
    /// here; or the store did not take the row. In the last two the action is not applied
    /// here either.
    pub(crate) fn place(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        action: NodeAction,
    ) -> Result<(), PlaceError> {
        if !self.links.contains_key(worker) {
            match self.known_elsewhere(worker) {
                Some(true) => {}
                Some(false) => return Err(UnknownNode(worker.as_str().to_owned()).into()),
                None => return Err(NodeUnread(worker.as_str().to_owned()).into()),
            }
        }
        let written = self.attempt("a cordon", &mut |rows| {
            rows.cordoned(worker, action.stored(rows.store_ms()))
        });
        if written != Attempt::Taken {
            return Err(PlaceError::Unwritten(worker.as_str().to_owned()));
        }
        let name = worker.clone();
        let event = match action {
            NodeAction::Cordon => Event::Cordon { worker: name },
            NodeAction::Drain(within) => Event::Drain {
                worker: name,
                deadline: now.saturating_add(within),
            },
            NodeAction::Uncordon => Event::Uncordon { worker: name },
        };
        tracing::info!(%worker, ?action, "operator action");
        self.feed_quiet(a, now, event);
        Ok(())
    }

    /// The nodes whose newest stream is open.
    pub(crate) fn connected(&self, a: &dyn Adapter) -> BTreeSet<WorkerId> {
        self.links
            .iter()
            .filter(|(worker, link)| a.stream_open(worker, link.stream))
            .map(|(worker, _)| worker.clone())
            .collect()
    }

    /// Reads the daemons of the other frontends from the `session/` rows, and shows again
    /// the stage of the operations that wait for a worker, now that it is known whether one
    /// of them could run each ([`Engine::republish_stuck`]). Nothing needs the list unless an
    /// operation waits, so with none the scan is skipped.
    pub(crate) fn look_at_remote_sessions(&mut self, a: &mut dyn Adapter) {
        if self.stuck.is_empty() {
            self.remote.clear();
            return;
        }
        let connected = self.connected(&*a);
        let mut remote = None;
        self.attempt("a session look", &mut |rows| {
            remote = Some(rows.remote_sessions(&connected)?);
            Ok(())
        });
        if let Some(remote) = remote {
            self.remote = remote;
            self.republish_stuck(a);
        }
    }
}
