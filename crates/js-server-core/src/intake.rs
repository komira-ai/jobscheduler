//! What a core takes in from the other frontends of its state store: the intake poll.
//!
//! Several frontends may share one store. Each schedules onto the daemons connected to it,
//! and the store, written conditionally, decides which of them runs an operation. A frontend
//! learns what the others did by looking at the store every [`INTAKE_POLL`]
//! ([`Core::intake`]):
//!
//! - **Epoch.** It reads the epoch row again. A core serves the epoch it started under (the
//!   operator restarts frontends after a bump), so a stored epoch past the served one is only
//!   reported ([`Core::epoch_ahead`]).
//! - **Queue.** An operation another frontend queued is taken in, with the `loss/`, `fate/`
//!   and parcel rows the store holds now (unless it holds a lease by then: its node's frontend
//!   owns it). An operation held queued whose `queue/` row is gone has been granted, finished
//!   or refused by another frontend: it leaves the scheduler (nothing is recorded as given up)
//!   and its callers are kept, to be answered from the store. One whose `queue/` row is a
//!   newer write than the one read before was granted and queued again meanwhile, and is
//!   taken in again with the rows of now. An operation whose requeue the store has not taken
//!   yet has no `queue/` row and stays.
//! - **Callers.** The callers of an operation another frontend took are shown what the store
//!   says of it each poll: executing while it holds a lease, and its result once the `done/`
//!   row is there; the callers of a result or a refusal this core could not write (it lost
//!   the race, or the node moved to another frontend) follow it the same way. They are kept
//!   for the finished retention after, as the callers of an operation this core finished are.
//! - **Joiners.** The callers of a request that joined an operation leased on a node another
//!   frontend owns are followed the same way from the start
//!   ([`Held::Elsewhere`](crate::Held::Elsewhere)): the scheduler never held the operation.
//! - **Watched operations.** The operations clients wait on or ask about by name that the
//!   scheduler does not hold are looked at each poll too ([`Adapter::watched`]).
//! - **A lost grant.** A grant this core wrote that the store refuses as stale was taken by
//!   another frontend: the placement is undone, and the operation follows the store as above.
//! - **Operator state.** The `cordon/` rows other frontends wrote since the last poll are
//!   applied (a drain's deadline is on the store's clock, so frontends with clocks of their
//!   own agree on when it ends), and a memory floor raised past this core's is raised here
//!   too.
//! - **Orphans.** A lease whose node has no session anywhere is requeued after
//!   [`Config::orphan_wait`](crate::Config).
//!
//! A core that is fenced by a handover, or whose store cannot be read, does nothing. One
//! whose intake is closed by a drain ([`Core::close_intake`]) takes in no operation of the
//! others' queue and does the rest.
//!
//! The poll is a call: the embedder runs it every [`INTAKE_POLL`] on its own timer and tells
//! the core the farm time.

use std::collections::BTreeMap;
use std::time::Duration;

use js_sched::{Cordon, Event, Input};
use js_state::Version;
use js_store::{Orphans, Rows, StateError, StoredCordon};
use js_types::sched::{FarmTime, OperationId, StateMachine, WaiterId, WorkerId};

use crate::adapter::{Adapter, Stage};
use crate::core::Core;
use crate::engine::Engine;
use crate::handover::note;
use crate::store::cordon_event;

/// How often the intake poll looks at the state store: often enough that an operation
/// another frontend queued waits a fifth of a second for a frontend with room, and a cordon
/// an operator set through one frontend stops the grants of the others as soon, and rare
/// enough that the scans it makes (the queue, the cordons, the floors, the leases and the
/// sessions) are a small share of the store's load.
pub const INTAKE_POLL: Duration = Duration::from_millis(200);

/// What one look at the state store finds that the intake poll acts on.
struct Polled {
    /// The epoch row.
    epoch: u64,
    /// The `queue/` rows: each queued operation, with the version of its row.
    queued: BTreeMap<OperationId, Version>,
    /// The `cordon/` rows.
    cordons: BTreeMap<WorkerId, StoredCordon>,
    /// The memory floors.
    floors: Vec<js_sched::Floor>,
    /// The store's clock, in milliseconds.
    at: u64,
}

impl Polled {
    /// Everything one intake poll reads.
    fn read(rows: &Rows) -> Result<Self, StateError> {
        Ok(Self {
            epoch: rows.stored_epoch()?,
            queued: rows.queued()?,
            cordons: rows.cordons()?.into_iter().collect(),
            floors: rows.floors()?,
            at: rows.store_ms(),
        })
    }
}

/// What a core keeps of the state store between two intake polls.
#[derive(Debug)]
pub(crate) struct Intake {
    /// The callers of operations another frontend took (or won the race to finish), by
    /// operation: the store answers them.
    elsewhere: BTreeMap<OperationId, Vec<WaiterId>>,
    /// The version of the `queue/` row of each operation held queued, as last read.
    queue_seen: BTreeMap<OperationId, Version>,
    /// The operations other frontends finished whose callers are kept, until when.
    kept: BTreeMap<OperationId, FarmTime>,
    /// The `cordon/` rows as the last poll (or the start) found them.
    pub(crate) cordons: BTreeMap<WorkerId, StoredCordon>,
    /// The leases whose node had no session row at the last sweep.
    orphans: Orphans,
    /// How long such a lease is left before it is requeued.
    orphan_wait: Duration,
    /// The stored epoch, if it is past the one this core serves.
    epoch_ahead: Option<u64>,
    /// How many polls have run to the end.
    polls: u64,
}

impl Intake {
    pub(crate) fn new(orphan_wait: Duration) -> Self {
        Self {
            elsewhere: BTreeMap::new(),
            queue_seen: BTreeMap::new(),
            kept: BTreeMap::new(),
            cordons: BTreeMap::new(),
            orphans: Orphans::default(),
            orphan_wait,
            epoch_ahead: None,
            polls: 0,
        }
    }

    /// Whether the callers of the finished `operation` are kept: another frontend finished
    /// it, and its retention has not passed.
    pub(crate) fn keeps(&self, operation: OperationId) -> bool {
        self.kept.contains_key(&operation)
    }
}

impl<A: Adapter> Core<A> {
    /// One intake poll (see the module documentation) at farm time `now`: takes in what the
    /// other frontends of the state store queued, forgets what they took, answers the callers
    /// of what they finished, applies the cordons and floors they set, and requeues the leases
    /// of nodes no frontend hears. Cheap when nothing changed. A core with no state store, and
    /// one a handover fenced, do nothing; so does one whose store cannot be read.
    ///
    /// Placement follows at the next tick.
    pub fn intake(&self, now: FarmTime) {
        let mut locked = self.lock();
        let crate::core::Locked { engine, adapter } = &mut *locked;
        engine.take_intake(adapter, now);
    }

    /// How many intake polls have run to the end: a poll of a fenced core counts (it ends at
    /// once, having taken nothing in), one that could not read the store does not, and nor
    /// does one of a core with no store. A test waits for polls by this count instead of for
    /// a time.
    #[must_use]
    pub fn intake_polls(&self) -> u64 {
        self.lock().engine.intake.polls
    }

    /// The epoch the state store holds, if it is past the one this core serves, as the last
    /// intake poll read it: the operator raised it ([`Core::bump_epoch`]), and this core
    /// serves the epoch it started under until it is restarted.
    #[must_use]
    pub fn epoch_ahead(&self) -> Option<u64> {
        self.lock().engine.intake.epoch_ahead
    }

    /// Applies the `cordon/` rows as the store holds them now, as an intake poll does, so a
    /// read of a node shows a cordon or drain another frontend wrote without waiting for the
    /// next poll. Returns the rows; `None`, with the nodes left as they are, when the core
    /// has no store or the store cannot be read.
    pub fn cordons_now(&self, now: FarmTime) -> Option<BTreeMap<WorkerId, StoredCordon>> {
        let mut locked = self.lock();
        let crate::core::Locked { engine, adapter } = &mut *locked;
        engine.take_cordons(adapter, now)
    }
}

impl Engine {
    /// [`Core::intake`] under the lock.
    pub(crate) fn take_intake(&mut self, a: &mut dyn Adapter, now: FarmTime) {
        if self.fenced {
            self.count_poll();
            return;
        }
        let mut polled = None;
        self.attempt("an intake poll", &mut |rows| {
            polled = Some(Polled::read(rows)?);
            Ok(())
        });
        let Some(polled) = polled else {
            return;
        };
        self.note_epoch(polled.epoch);
        self.release_followed(a, now);
        self.take_queued(a, &polled.queued);
        self.follow_elsewhere(a, now);
        self.follow_watched(a);
        self.apply_cordons(a, now, polled.cordons, polled.at);
        self.apply_floors(&polled.floors);
        self.sweep_orphans(a);
        self.look_at_remote_sessions(a);
        self.count_poll();
    }

    /// Counts one poll that ran to the end ([`Core::intake_polls`]).
    fn count_poll(&mut self) {
        self.intake.polls = self.intake.polls.saturating_add(1);
    }

    /// Reports the stored epoch `stored` once when it is past the one this core serves.
    fn note_epoch(&mut self, stored: u64) {
        let served = self.epoch.unwrap_or_default();
        let ahead = Some(stored).filter(|epoch| *epoch > served);
        if let Some(epoch) = ahead.filter(|_| ahead != self.intake.epoch_ahead) {
            tracing::warn!(
                stored = epoch,
                served,
                "the state store holds a newer epoch than this frontend serves: restart it"
            );
        }
        self.intake.epoch_ahead = ahead;
    }

    /// Forgets the callers of the operations other frontends finished whose retention has
    /// passed.
    fn release_followed(&mut self, a: &mut dyn Adapter, now: FarmTime) {
        self.intake.kept.retain(|_, until| now < *until);
        self.forget_dropped(a);
    }

    /// Makes the scheduler's queue the store's: reconciles each operation held queued with
    /// its `queue/` row in `queued`, then takes in the queued operations it does not hold.
    fn take_queued(&mut self, a: &mut dyn Adapter, queued: &BTreeMap<OperationId, Version>) {
        let held: Vec<OperationId> = self.sched.queued().collect();
        for operation in held {
            self.reconcile(a, operation, queued.get(&operation).copied());
        }
        // A core whose intake is closed ([`Core::close_intake`]) takes in nothing new.
        if !self.closed {
            for (&operation, &version) in queued {
                if !self.known_operation(operation) {
                    self.take_in(a, operation, version);
                }
            }
        }
        self.intake
            .queue_seen
            .retain(|operation, _| queued.contains_key(operation));
    }

    /// What the store says of `operation`, which the scheduler holds queued, given the
    /// version of its `queue/` row (`None` if it has none).
    ///
    /// No row: another frontend took it, and it leaves the scheduler; unless the scheduler
    /// gave a lease of it up and the store has not taken the requeue yet, when the row is
    /// still to come. A row that is not the one read before: the operation was granted and
    /// queued again, and is taken in again with the rows of now, unless the intake is closed:
    /// then it is only handed off.
    pub(crate) fn reconcile(
        &mut self,
        a: &mut dyn Adapter,
        operation: OperationId,
        row: Option<Version>,
    ) {
        let Some(version) = row else {
            if !self.requeue_pending(operation) {
                self.hand_off(operation);
            }
            return;
        };
        let before = self.intake.queue_seen.insert(operation, version);
        if before.is_some_and(|before| before != version) {
            self.hand_off(operation);
            if !self.closed {
                self.take_in(a, operation, version);
            }
        }
    }

    /// Forgets `operation`, which another frontend took, and keeps its callers.
    fn hand_off(&mut self, operation: OperationId) {
        let callers = self.sched.forget_queued(operation).unwrap_or_default();
        self.keep_following(operation, callers);
    }

    /// Keeps `callers`, who wait for `operation`, to be answered from the store.
    pub(crate) fn keep_following(&mut self, operation: OperationId, callers: Vec<WaiterId>) {
        let followed = self.intake.elsewhere.entry(operation).or_default();
        followed.extend(callers);
    }

    /// Takes in the queued `operation`, whose `queue/` row is at `version`, with the rows the
    /// store holds of it now. An operation that has no `op/` row any more (it finished
    /// meanwhile) is not taken in, and neither is one that holds a lease now: another
    /// frontend granted it since the poll read the queue, and its lease is taken up only when
    /// its node says `Hello` here.
    fn take_in(&mut self, a: &mut dyn Adapter, operation: OperationId, version: Version) {
        let mut found = None;
        self.attempt("a queued operation", &mut |rows| {
            found = rows.stored_operation(operation)?;
            Ok(())
        });
        found
            .filter(|stored| stored.lease.is_none())
            .into_iter()
            .for_each(|stored| {
                self.take_in_stored(a, stored);
                self.intake.queue_seen.insert(operation, version);
            });
    }

    /// Shows the callers kept for the operations other frontends took what the store says of
    /// each: executing while it holds a lease, its result once it has one. The callers of an
    /// operation that ended, or whose own listeners are all gone, are kept for the finished
    /// retention and then forgotten.
    fn follow_elsewhere(&mut self, a: &mut dyn Adapter, now: FarmTime) {
        for (operation, callers) in std::mem::take(&mut self.intake.elsewhere) {
            let live = a.waiting_among(&callers);
            if live.is_empty() {
                self.retire(a, operation, callers, now);
                continue;
            }
            let mut looked = None;
            self.attempt("a followed operation", &mut |rows| {
                looked = Some(rows.progress(operation)?);
                Ok(())
            });
            let untraced = &mut self.untraced_kept;
            let stage = looked.and_then(|progress| note(untraced, operation, &progress));
            if let Some(stage) = &stage {
                a.show_callers(&live, stage);
            }
            if let Some(Stage::Done(ended)) = &stage {
                a.followed(operation, ended);
                self.retire(a, operation, callers, now);
            } else {
                self.intake.elsewhere.insert(operation, callers);
            }
        }
    }

    /// Ends the following of `operation`: its `callers` are kept for the finished retention,
    /// as the callers of an operation this core finished are.
    fn retire(
        &mut self,
        a: &mut dyn Adapter,
        operation: OperationId,
        callers: Vec<WaiterId>,
        now: FarmTime,
    ) {
        self.untraced_kept.remove(&operation);
        let until = now.saturating_add(self.finished_retention);
        self.intake.kept.insert(operation, until);
        a.keep_finished(operation, callers);
        self.forget_dropped(a);
    }

    /// Places the nodes as the `cordon/` rows `rows` say now, `at` milliseconds on the
    /// store's clock: a row that is not as the last poll found it is applied, and a node
    /// whose row is gone returns to placement.
    fn apply_cordons(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        rows: BTreeMap<WorkerId, StoredCordon>,
        at: u64,
    ) {
        let seen = std::mem::replace(&mut self.intake.cordons, rows.clone());
        for (worker, cordon) in &rows {
            if seen.get(worker) != Some(cordon) {
                tracing::info!(%worker, ?cordon, "another frontend placed the node");
                self.feed_quiet(a, now, cordon_event(worker.clone(), *cordon, now, at));
            }
        }
        for worker in seen.keys().filter(|worker| !rows.contains_key(*worker)) {
            tracing::info!(%worker, "another frontend returned the node to placement");
            let worker = worker.clone();
            self.feed_quiet(a, now, Event::Uncordon { worker });
        }
    }

    /// [`Core::cordons_now`] under the lock.
    fn take_cordons(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
    ) -> Option<BTreeMap<WorkerId, StoredCordon>> {
        let mut read = None;
        self.attempt("a cordon look", &mut |rows| {
            read = Some((rows.cordons()?, rows.store_ms()));
            Ok(())
        });
        let (rows, at) = read?;
        let rows: BTreeMap<WorkerId, StoredCordon> = rows.into_iter().collect();
        self.apply_cordons(a, now, rows.clone(), at);
        Some(rows)
    }

    /// Puts `worker` back in the drain its `cordon/` row says, if the drain ended for want of
    /// a lease. A start takes up no leased operation, so a drain taken up at the start finds
    /// its node holding no lease and ends; the node's leases are taken up when it says
    /// `Hello` ([`Engine::claim_worker`], which calls this after taking them up), and a node
    /// that runs work is not drained.
    pub(crate) fn resume_drain(&mut self, now: FarmTime, worker: &WorkerId) {
        if self.sched.cordon(worker) != Some(&Cordon::Drained) {
            return;
        }
        let mut stored = None;
        self.attempt("a cordon look", &mut |rows| {
            stored = Some((rows.cordons()?, rows.store_ms()));
            Ok(())
        });
        let drain = stored.and_then(|(rows, at)| {
            let row = rows.into_iter().find(|(named, _)| named == worker);
            row.map(|(named, cordon)| cordon_event(named, cordon, now, at))
        });
        drain.into_iter().for_each(|event| {
            let none = self.sched.apply(Input::new(now, event));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        });
    }

    /// Raises the memory floors the scheduler holds lower than the store's: another
    /// frontend's kills raised them.
    fn apply_floors(&mut self, floors: &[js_sched::Floor]) {
        for floor in floors {
            self.sched.raise_floor(floor);
        }
    }

    /// Requeues the leases whose node has no session row anywhere (see
    /// [`js_store::Rows::sweep`]).
    fn sweep_orphans(&mut self, a: &dyn Adapter) {
        let connected = self.connected(a);
        let wait = self.intake.orphan_wait;
        let mut orphans = std::mem::take(&mut self.intake.orphans);
        self.attempt("an orphan sweep", &mut |rows| {
            rows.sweep(wait, &connected, &mut orphans)
        });
        self.intake.orphans = orphans;
    }
}
