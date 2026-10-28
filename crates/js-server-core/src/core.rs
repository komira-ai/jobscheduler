//! The core: one lock around the scheduler's driver and the server's own state.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use js_sched::{DaemonInstance, Event, FINISHED_RETENTION, Request, UNSERVABLE_WAIT};
use js_store::ORPHAN_WAIT;
use js_store::{EpochBump, Progress, StateError};
use js_types::sched::{FarmTime, LeaseId, OperationId, WaiterId, WorkerId};

use crate::adapter::{Adapter, Delivery, View};
use crate::admission::{Admission, AllowAll, Denied};
use crate::engine::Engine;
use crate::host::{Beat, Reported, Stream};
use crate::observer::Observer;
use crate::report::{Report, Standing, Taken, log_refusal};
use crate::store::Storage;
use crate::write::Writer;

/// How a core is built.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// The scheduler term: every lease the core grants carries it, and a lease id names one
    /// lease ever when each process takes a term no earlier one did.
    pub term: u64,
    /// How long queued work that no live worker can run waits before it is refused.
    pub unservable_wait: Duration,
    /// How long a finished operation is kept, which a late request for its result still
    /// finds.
    pub finished_retention: Duration,
    /// How long a lease whose node has no session row anywhere is left before the intake
    /// poll requeues it ([`Core::intake`]).
    pub orphan_wait: Duration,
}

impl Config {
    /// The scheduler's defaults, under `term`.
    #[must_use]
    pub const fn new(term: u64) -> Self {
        Self {
            term,
            unservable_wait: UNSERVABLE_WAIT,
            finished_retention: FINISHED_RETENTION,
            orphan_wait: ORPHAN_WAIT,
        }
    }

    /// This configuration, refusing unservable work after `wait`.
    #[must_use]
    pub const fn with_unservable_wait(mut self, wait: Duration) -> Self {
        self.unservable_wait = wait;
        self
    }

    /// This configuration, requeueing a lease whose node has no session row after `wait`.
    #[must_use]
    pub const fn with_orphan_wait(mut self, wait: Duration) -> Self {
        self.orphan_wait = wait;
        self
    }

    /// This configuration, keeping a finished operation for `retention`.
    #[must_use]
    pub const fn with_finished_retention(mut self, retention: Duration) -> Self {
        self.finished_retention = retention;
        self
    }
}

/// Where an admitted request is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// By this core's scheduler: the request is a caller of an operation it holds.
    Here,
    /// By another frontend of the state store: the request joined this operation, which
    /// holds a lease on a node that frontend owns. This core takes up nothing of it, not
    /// even the lease (that would end in the scheduler giving up a lease it does not own):
    /// the request follows the operation through the store until its result is there
    /// ([`Core::progress`]).
    Elsewhere(OperationId),
}

/// The caller, and the operation, a submit became.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admitted {
    /// The caller, numbered by the core.
    pub waiter: WaiterId,
    /// The operation: the twin the request joined, or a new one.
    pub operation: OperationId,
    /// The request joined an operation already in flight.
    pub joined: bool,
    /// Where the request is held.
    pub held: Held,
}

/// A request that was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotQueued {
    /// The core is handing its work to a successor.
    #[error("this frontend is handing its work to its successor")]
    Fenced,
    /// The state store did not take the request: nothing is queued, and the client, who is
    /// told so, asks again.
    #[error("the scheduler's state store did not take the request")]
    Unavailable,
    /// The store holds the work's previous run, whose finish is written but not yet taken
    /// out of the rows: the request asks again, and queues afresh once it is.
    #[error("the work's previous run is finishing; the request is not queued")]
    Finishing,
    /// A newer writer holds the state store: this core writes nothing more, and takes no
    /// work.
    #[error("this scheduler was superseded by a newer writer of the state store")]
    Superseded,
}

/// An operator action named a node that never registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no node {0} has registered")]
pub struct UnknownNode(pub String);

/// A node another frontend of the state store holds could not be looked at, because the
/// store could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the state store could not be read for node {0}, which another frontend holds")]
pub struct NodeUnread(pub String);

/// An operator action on a node that was not applied.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlaceError {
    /// No frontend of the state store has held the node.
    #[error(transparent)]
    Unknown(#[from] UnknownNode),
    /// The store could not say whether another frontend holds the node.
    #[error(transparent)]
    Unread(#[from] NodeUnread),
    /// The store did not take the node's `cordon/` row: the action was not applied here
    /// either, and the other frontend that holds the node never sees it.
    #[error("the state store could not be written for node {0}; the action was not applied")]
    Unwritten(String),
}

/// What an operator does to a node's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    /// Offer it no new lease; its leases run on.
    Cordon,
    /// Cordon it and wait this long for its leases to end; then the drain pauses.
    Drain(Duration),
    /// Return it to placement, ending any drain.
    Uncordon,
}

/// What the lock covers: the engine, and the adapter it calls.
pub(crate) struct Locked<A> {
    pub(crate) engine: Engine,
    pub(crate) adapter: A,
}

/// The server's core. Cheap to share: every method takes `&self`.
///
/// Everything that changes scheduler state happens under one lock, in the order inputs
/// arrive, and the scheduler's effects are carried out under the same lock (`Commit`,
/// `Start`, the offer that precedes a `Start`) or right after it (an answer's
/// [`Delivery`]). The lock covers the [`Adapter`]'s state too, as a core with the adapter
/// behind a lock of its own would order a caller's registration and the feed that follows
/// as two steps.
///
/// Time is an input: each method that feeds the scheduler takes the farm time `now`, and a
/// caller that reads a clock reads it before the call. Generic only in this shell: all the
/// logic is in a type that is not, so a test binary's run of it counts toward one
/// instantiation.
pub struct Core<A: Adapter> {
    state: Mutex<Locked<A>>,
    admission: Box<dyn Admission>,
    observer: Box<dyn Observer>,
}

impl<A: Adapter> Core<A> {
    /// A core for `config`, over `adapter`, hearing every host, writing nothing, and
    /// observed by nobody.
    #[must_use]
    pub fn new(config: &Config, adapter: A) -> Self {
        Self {
            state: Mutex::new(Locked {
                engine: Engine::new(config),
                adapter,
            }),
            admission: Box::new(AllowAll),
            observer: Box::new(()),
        }
    }

    /// A core over the state store `storage`, which takes up what the store holds
    /// ([`Adapter::adopted`] is told of each operation). Every change of the scheduler is
    /// written to the store before it is acted on: a request before it is queued, a grant
    /// before its `Start` leaves, a result before its callers are answered. The scheduler's
    /// term is the store's epoch, which [`Storage::with_epoch`] decides; `config.term` names
    /// this frontend in the store, and is the epoch of a store that has none. `now` is the
    /// farm time of the start: the lease grace of a lease the store holds runs from it.
    ///
    /// The start takes up the operations that wait for a lease, and none that holds one: the
    /// lease belongs to the frontend its node is connected to, which may be another than this
    /// one, and a lease taken up here would be given up once the lease grace had passed, and
    /// its operation run twice. A leased operation is taken up when its node says `Hello`
    /// here ([`Core::register`]); if the node never does, the orphan sweep of the intake poll
    /// ([`Core::intake`]) requeues its lease. An operator's cordons are taken up too.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds a row that is not one the scheduler
    /// wrote.
    pub fn open(
        config: &Config,
        adapter: A,
        storage: Storage,
        now: FarmTime,
    ) -> Result<Self, StateError> {
        let mut core = Self::new(config, adapter);
        let Locked { engine, adapter } = core.state_mut();
        engine.keep_in(adapter, config.term, storage, now)?;
        Ok(core)
    }

    /// This core, hearing the hosts `admission` allows.
    #[must_use]
    pub fn with_admission(mut self, admission: impl Admission + 'static) -> Self {
        self.admission = Box::new(admission);
        self
    }

    /// This core, reporting itself to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: impl Observer + 'static) -> Self {
        self.observer = Box::new(observer);
        self
    }

    /// This core, whose committed records' rows `writer` takes before their effects are
    /// carried out.
    #[must_use]
    pub fn with_writer(mut self, writer: impl Writer + 'static) -> Self {
        self.state_mut().engine.writer = Box::new(writer);
        self
    }

    /// This core, holding at most `cap` committed records for a writer that refuses them
    /// (instead of [`HELD_CAP`](crate::HELD_CAP)); a record past it is dropped.
    #[must_use]
    pub fn with_held_cap(mut self, cap: usize) -> Self {
        self.state_mut().engine.held_cap = cap;
        self
    }

    fn state_mut(&mut self) -> &mut Locked<A> {
        // Nothing under the lock panics midway through a change.
        self.state.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Locked<A>> {
        // Nothing under the lock panics midway through a change.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `request`, or joins it to the operation already in flight for it, for a
    /// caller the core numbers. `enrol` runs under the lock, before the scheduler hears of
    /// the request: the adapter records the caller there, so the operation's first `Start`
    /// finds it. The time a submit waited for the lock goes to the [`Observer`]; no other
    /// method reports one.
    ///
    /// A core over a store writes the request first, with `parcel` (what the process that
    /// takes the operation up after a restart is told of it, [`Adapter::adopted`]): it is
    /// queued only once the store has it, and a request for work the store holds an
    /// operation for joins that operation. An operation that holds a lease on a node
    /// another frontend owns is not taken up ([`Held::Elsewhere`]): the scheduler hears
    /// nothing of the request, and its caller is followed through the store
    /// ([`Core::intake`]). A core with no store ignores `parcel`.
    ///
    /// # Errors
    /// The core is fenced, or the store did not take the request: nothing is queued.
    pub fn submit(
        &self,
        now: FarmTime,
        request: Request,
        parcel: &[u8],
        enrol: impl FnOnce(&mut A, &Admitted),
    ) -> Result<Admitted, NotQueued> {
        let waiting = Instant::now();
        let mut locked = self.lock();
        self.observer.lock_waited(waiting.elapsed());
        let Locked { engine, adapter } = &mut *locked;
        let admitted = engine.admit(adapter, &request, parcel, now)?;
        enrol(adapter, &admitted);
        match admitted.held {
            Held::Here => {
                let event = Event::Submit {
                    waiter: admitted.waiter,
                    request,
                };
                engine.feed_quiet(adapter, now, event);
            }
            // The operation is another frontend's: this scheduler never holds it, and the
            // caller is shown what the store says of it at the intake polls.
            Held::Elsewhere(operation) => engine.keep_following(operation, vec![admitted.waiter]),
        }
        Ok(admitted)
    }

    /// The first `Hello` of a stream, from the daemon process `instance`: numbers the
    /// stream, makes it the worker's newest for every `Start` from now on, and registers
    /// the worker (a new session) with what it `reported`. Its phase is taken before any
    /// placement, and may send a `Start` again or a `Cancel` on this stream. Messages still
    /// arriving on its earlier stream are ignored from now on. A stream of another process
    /// than the earlier stream's is logged: leases that process ran are kept until it has
    /// fenced, unless it is proven stopped. It is when the new process names it as its
    /// `predecessor` (whose runs the new daemon's drivers killed before its `Hello`) and the
    /// earlier stream has closed ([`Adapter::stream_open`]): a process still connected may
    /// be another machine's, with a copy of this one's disk.
    ///
    /// `open` runs under the lock, after the stream is numbered and before the scheduler
    /// hears of the worker: the adapter attaches the connection there, so the `Start`s the
    /// registration leads to find it.
    ///
    /// # Errors
    /// The admission refuses the host.
    pub fn register(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        instance: DaemonInstance,
        predecessor: DaemonInstance,
        reported: Reported,
        open: impl FnOnce(&mut A, Stream),
    ) -> Result<Stream, Denied> {
        self.admission.allow_host(worker, &reported)?;
        let mut locked = self.lock();
        let Locked { engine, adapter } = &mut *locked;
        let opened = engine.open_stream(adapter, worker, &instance, &predecessor);
        open(adapter, opened.stream);
        engine.register(adapter, now, worker, instance, opened, reported);
        Ok(opened.stream)
    }

    /// A `Hello` resent on `stream`: the node report or the node's phase changed. Changes
    /// the worker's capacity, capabilities and phase, and opens no session; ignored if
    /// `stream` was replaced (and `update` does not run). `update` runs under the lock,
    /// before the scheduler hears of the change.
    ///
    /// # Errors
    /// The admission refuses the host.
    pub fn resize(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        stream: Stream,
        reported: Reported,
        update: impl FnOnce(&mut A),
    ) -> Result<(), Denied> {
        self.admission.allow_host(worker, &reported)?;
        let mut locked = self.lock();
        let Locked { engine, adapter } = &mut *locked;
        if engine.is_current(worker, stream) {
            update(adapter);
            engine.resize(adapter, now, worker, reported);
        }
        Ok(())
    }

    /// Heartbeat `beat` on `stream`. Returns whether it was taken and is to be
    /// acknowledged: a heartbeat from a replaced stream is dropped. Each lease it lists
    /// that the scheduler no longer holds on `worker` is sent to [`Adapter::cancel`]. A
    /// lease it leaves out that a proven restart lost past its operation's farm reruns
    /// finishes the operation: its callers are answered.
    ///
    /// # Errors
    /// The admission refuses the host.
    pub async fn heartbeat(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        stream: Stream,
        beat: Beat,
    ) -> Result<bool, Denied> {
        self.admission.allow_beat(worker, &beat)?;
        let (settled, acknowledged) = {
            let mut locked = self.lock();
            let Locked { engine, adapter } = &mut *locked;
            let Some(answers) = engine.take_heartbeat(adapter, now, worker, stream, beat) else {
                return Ok(false);
            };
            // A node this core does not own is not kept alive in the store by it, and is
            // not acknowledged (though the scheduler hears it) while the core has not
            // written its session row lately, so a daemon that the other frontends cannot
            // see alive, or that has moved, fences on time.
            let acknowledged = engine.acknowledge(now, worker);
            let settled = answers
                .iter()
                .map(|answer| engine.settle(adapter, answer))
                .collect::<Vec<_>>();
            (settled, acknowledged)
        };
        deliver(settled).await;
        Ok(acknowledged)
    }

    /// What `lease` stands for as `worker`'s `Result` names it: the operation, if `worker`
    /// holds its current lease; else why the `Result` is not taken. A copy of a result the
    /// scheduler took, whose finish waits to be written, is acknowledged with the first.
    ///
    /// # Errors
    /// The admission refuses the host.
    pub fn standing(&self, worker: &WorkerId, lease: LeaseId) -> Result<Standing, Denied> {
        self.admission.allow_result(worker, lease)?;
        let mut locked = self.lock();
        let standing = locked.engine.standing(lease, worker);
        if standing == Standing::Held {
            locked.engine.owed.entry(lease).or_insert(worker.clone());
        }
        if let Standing::Refused(reason) = standing {
            log_refusal(worker, lease, reason);
        }
        Ok(standing)
    }

    /// Feeds `worker`'s `report`, and answers the callers of the operation it finishes. The
    /// report is checked first ([`Core::standing`]); `park` runs under the lock before the
    /// feed, so the adapter keeps what the answer needs of the report (the result it
    /// carries), and what no answer used is dropped again after. Returns whether the
    /// scheduler took it, and so what to acknowledge it with.
    pub async fn report(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        report: Report,
        park: impl FnOnce(&mut A),
    ) -> Taken {
        let (settled, taken) = {
            let mut locked = self.lock();
            let Locked { engine, adapter } = &mut *locked;
            // Parked before the feed: the answer it decides is settled with it. An answer
            // held back earlier, released by this input, is settled with its own.
            park(adapter);
            let (answers, taken) = engine.take_report(adapter, now, worker, report);
            let settled: Vec<Delivery> = answers
                .iter()
                .map(|answer| engine.settle(adapter, answer))
                .collect();
            // What no answer used waits only if its answer does.
            engine.forget_parked(adapter);
            (settled, taken)
        };
        deliver(settled).await;
        taken
    }

    /// Expires leases of silent workers and places queued work. A silent lease lost past
    /// its operation's farm reruns finishes the operation: its callers are answered, as a
    /// heartbeat's are. Logs each drain that has paused at its deadline, once.
    pub async fn tick(&self, now: FarmTime) {
        let settled = {
            let mut locked = self.lock();
            let Locked { engine, adapter } = &mut *locked;
            let answers = engine.tick(adapter, now);
            answers
                .iter()
                .map(|answer| engine.settle(adapter, answer))
                .collect::<Vec<_>>()
        };
        deliver(settled).await;
    }

    /// Cordons, drains or uncordons `worker` (see `js_sched::Cordon`). A drain's deadline
    /// counts from `now`. A core over a store writes the node's `cordon/` row first, and a
    /// node connected to another frontend of the store is placed too: that frontend applies
    /// the row.
    ///
    /// # Errors
    /// `worker` has never registered with this core, and no frontend of the store has
    /// claimed it; the store cannot say whether one has, and `worker` is not connected
    /// here; or the store did not take the node's row. In the last two the action is not
    /// applied here either.
    pub fn place(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        action: NodeAction,
    ) -> Result<(), PlaceError> {
        let mut locked = self.lock();
        let Locked { engine, adapter } = &mut *locked;
        engine.place(adapter, now, worker, action)
    }

    /// Takes this core out of service for good, for a successor to take over: from now on
    /// it feeds the scheduler no input (a `Result` is refused, nothing is granted or given
    /// up, a heartbeat changes nothing) and queues no request. What it already holds stays
    /// as it was.
    pub fn fence(&self) {
        self.lock().engine.fenced = true;
    }

    /// Lets a core that [`Core::fence`] stopped for a takeover that did not happen feed the
    /// scheduler again. Nothing was fed while it was fenced, so the core's memory is as of
    /// the fence.
    pub fn resume(&self) {
        self.lock().engine.fenced = false;
    }

    /// Closes this core's intake, the step of a drain before the fence: from now on it
    /// places no work on its nodes (each is cordoned in the scheduler alone), so the
    /// operations it holds queued are left for the other frontends. It goes on with
    /// everything else: a `Result` and a heartbeat of a lease it holds are taken, and its
    /// callers are answered, from its own nodes and from what the other frontends write
    /// ([`Core::intake`]); the intake poll takes in no operation another frontend queued.
    /// Nothing undoes it.
    pub fn close_intake(&self) {
        self.lock().engine.closed = true;
    }

    /// Operations waiting in the scheduler's queue for a lease, now.
    #[must_use]
    pub fn queued_operations(&self) -> u64 {
        self.lock().engine.sched.queued_len() as u64
    }

    /// Writes the core gave up on for good: held effects dropped, and records over the
    /// cap.
    #[must_use]
    pub fn dropped_writes(&self) -> u64 {
        self.lock().engine.write_drops
    }

    /// Workers that have a stream, open or since closed.
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.lock().engine.links.len()
    }

    /// The effects of committed records the state store has not taken yet, and so are not
    /// carried out.
    #[must_use]
    pub fn write_behind(&self) -> usize {
        let locked = self.lock();
        locked
            .engine
            .unwritten
            .iter()
            .map(|b| b.effects.len())
            .sum()
    }

    /// Whether the scheduler has been told the state store is unavailable: nothing is
    /// granted or given up until a write takes again.
    #[must_use]
    pub fn store_down(&self) -> bool {
        self.lock().engine.store_down
    }

    /// The epoch the state store was opened under, if the core has a store.
    #[must_use]
    pub fn epoch(&self) -> Option<u64> {
        self.lock().engine.epoch
    }

    /// Where `operation` stands in the state store, as a process that did not run it sees
    /// it; `None` if the core has no store.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn progress(&self, operation: OperationId) -> Result<Option<Progress>, StateError> {
        let locked = self.lock();
        let rows = locked.engine.rows.as_ref();
        rows.map(|rows| rows.progress(operation)).transpose()
    }

    /// The value of the `done/` row of `operation`, if the core has a store and the row is
    /// still there: the answer a process that did not finish the operation gives the
    /// callers it kept waiting.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn ended(&self, operation: OperationId) -> Result<Option<Vec<u8>>, StateError> {
        let locked = self.lock();
        let rows = locked.engine.rows.as_ref();
        rows.map_or(Ok(None), |rows| rows.ended(operation))
    }

    /// Raises the epoch the store holds by one, and returns the epoch before and the one
    /// stored now; `None` if the core has no store. This core keeps the epoch it started
    /// under: the stored one is read by a core when it starts, so the daemons drop the leases
    /// of the older epoch when they reach a core started after the bump.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds no epoch.
    pub fn bump_epoch(&self) -> Result<Option<EpochBump>, StateError> {
        let locked = self.lock();
        let rows = locked.engine.rows.as_ref();
        rows.map(|rows| rows.bump_epoch()).transpose()
    }

    /// Reads the daemons of the other frontends from the store's session rows, and shows
    /// again the stage of the operations that wait for a worker, now that it is known
    /// whether one of them could run each. Cheap when no operation waits.
    pub fn refresh_remote(&self) {
        let mut locked = self.lock();
        let Locked { engine, adapter } = &mut *locked;
        engine.look_at_remote_sessions(adapter);
    }

    /// Runs `read` under the lock with the scheduler and the newest stream of each worker
    /// as they stand, and the adapter: for an operator's view of the farm, which the
    /// adapter composes from its own state and the scheduler's.
    pub fn with<R>(&self, read: impl FnOnce(&View<'_>, &mut A) -> R) -> R {
        let mut locked = self.lock();
        let Locked { engine, adapter } = &mut *locked;
        read(&engine.view(), adapter)
    }
}

/// Runs the work the answers left, in order, once the lock is released.
async fn deliver(settled: Vec<Delivery>) {
    for delivery in settled {
        delivery.await;
    }
}
