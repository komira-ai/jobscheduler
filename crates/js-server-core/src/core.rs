//! The core: one lock around the scheduler's driver and the server's own state.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use js_sched::{DaemonInstance, Event, FINISHED_RETENTION, Request, UNSERVABLE_WAIT};
use js_types::sched::{FarmTime, OperationId, WaiterId, WorkerId};

use crate::adapter::{Adapter, Delivery, View};
use crate::admission::{Admission, AllowAll, Denied};
use crate::engine::Engine;
use crate::host::{Beat, Reported, Stream};
use crate::observer::Observer;
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
}

impl Config {
    /// The scheduler's defaults, under `term`.
    #[must_use]
    pub const fn new(term: u64) -> Self {
        Self {
            term,
            unservable_wait: UNSERVABLE_WAIT,
            finished_retention: FINISHED_RETENTION,
        }
    }

    /// This configuration, refusing unservable work after `wait`.
    #[must_use]
    pub const fn with_unservable_wait(mut self, wait: Duration) -> Self {
        self.unservable_wait = wait;
        self
    }

    /// This configuration, keeping a finished operation for `retention`.
    #[must_use]
    pub const fn with_finished_retention(mut self, retention: Duration) -> Self {
        self.finished_retention = retention;
        self
    }
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
}

/// A request refused because the core is handing its work to a successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("this frontend is handing its work to its successor")]
pub struct Fenced;

/// An operator action named a node that never registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no node {0} has registered")]
pub struct UnknownNode(pub String);

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
struct Locked<A> {
    engine: Engine,
    adapter: A,
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

    fn lock(&self) -> MutexGuard<'_, Locked<A>> {
        // Nothing under the lock panics midway through a change.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `request`, or joins it to the operation already in flight for it, for a
    /// caller the core numbers. `enrol` runs under the lock, before the scheduler hears of
    /// the request: the adapter records the caller there, so the operation's first `Start`
    /// finds it. The time a submit waited for the lock goes to the [`Observer`]; no other
    /// method reports one.
    ///
    /// # Errors
    /// The core is fenced: nothing is queued.
    pub fn submit(
        &self,
        now: FarmTime,
        request: Request,
        enrol: impl FnOnce(&mut A, &Admitted),
    ) -> Result<Admitted, Fenced> {
        let waiting = Instant::now();
        let mut locked = self.lock();
        self.observer.lock_waited(waiting.elapsed());
        let Locked { engine, adapter } = &mut *locked;
        let admitted = engine.admit(&request)?;
        enrol(adapter, &admitted);
        let event = Event::Submit {
            waiter: admitted.waiter,
            request,
        };
        engine.feed_quiet(adapter, now, event);
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
        let settled = {
            let mut locked = self.lock();
            let Locked { engine, adapter } = &mut *locked;
            let Some(answers) = engine.take_heartbeat(adapter, now, worker, stream, beat) else {
                return Ok(false);
            };
            answers
                .iter()
                .map(|answer| engine.settle(adapter, answer))
                .collect::<Vec<_>>()
        };
        deliver(settled).await;
        Ok(true)
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
    /// counts from `now`.
    ///
    /// # Errors
    /// `worker` has never registered.
    pub fn place(
        &self,
        now: FarmTime,
        worker: &WorkerId,
        action: NodeAction,
    ) -> Result<(), UnknownNode> {
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
    /// callers are answered. Nothing undoes it.
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
