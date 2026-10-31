//! The lifecycle's own thread.
//!
//! Every call into the container runtime blocks: an image pull, the grace of a stop, a
//! daemon that is slow to answer. The session must never wait for one, or a slow engine
//! would make the agent miss its heartbeats and the server would give up the host. So the
//! [`Lifecycle`] lives on a thread of its own, made by [`Worker::start`], and the session
//! reaches it only through this module:
//!
//! - **Replies go in through a bounded queue** ([`QUEUE`] orders). [`Worker::try_deliver`]
//!   never waits: when the worker is busy and the queue is full the reply is dropped and
//!   the caller says so. Nothing is lost for good: the server resends a start until the
//!   run is listed, a stop until it leaves the held list, a cancel for as long as the host
//!   lists the run, the acknowledgement of every result the heartbeat still carries and
//!   the acknowledgement of every heartbeat that carries the taint.
//! - **State comes out as a snapshot.** After every order and every step the worker
//!   publishes the held runs, the results not yet acknowledged and the taint to report
//!   ([`Snapshot`]); [`Worker::snapshot`] copies it under a lock that is held only to
//!   swap it, never across a runtime call.
//! - **The worker steps by itself.** It calls [`Lifecycle::step`] after each order and
//!   whenever it has had none for the step interval of its [`Plan`], so a container that
//!   exited is found whether or not the server said anything.
//! - **The runtime is made on the worker thread**, by the [`Machine`] of the plan, so it
//!   needs no `Send` bound. The lifecycle is built (and the containers the previous
//!   process left are adopted) before [`Worker::start`] returns, so the first hello can
//!   list them.
//! - **The worker samples the usage** of the leases that run, once per heartbeat interval,
//!   when `--actions-cgroup` is given: after a step it asks its [`Collector`] whether a
//!   window has passed and, if so, reads the cgroup files on its own thread and publishes
//!   the reading in the [`Outbox`] the session takes it from. The session never reads a
//!   cgroup file.
//! - **A whole-node lease is prepared on the worker's thread.** The post-job checks and the
//!   readiness hook block, so the session asks through [`WorkerHost`]: it sends a prepare or
//!   restore order without waiting and reads the last answer the worker published, which
//!   is tagged with the lease it is about. An answer for another lease is never handed
//!   back; the worker drops its answers when its lifecycle follows a different lease. The
//!   hook runs here, once for each grant (and again after an agent restart that holds no run
//!   of the lease, which prepares it afresh; a lease taken over with a run held runs none).
//!   Because the hook runs on the worker's thread it holds the worker for as long as it
//!   runs, up to its bound (twice `prepare_timeout`): no reply is applied meanwhile, so
//!   starts, stops and acknowledgements wait in the queue (a start's validity counts from
//!   its arrival and may pass), and replies beyond the queue are dropped and sent again.
//!   Heartbeats go on. The watchdog allows the worker that long for the hook, only while the
//!   hook runs: its limit grows by the bound when the hook is about to start, and when the hook
//!   returns it is the order's plain limit moved later by the time the hook really took
//!   ([`HookWindow`]), so the rest of the order has the plain stall bound counted from its
//!   own start, less nothing the hook used.
//! - **What the worker does is logged by the session thread**: the lifecycle's events
//!   are kept as lines ([`Worker::take_lines`]; at most [`LINES_CAP`] wait, the rest are
//!   counted and dropped) and the loop around the session prints them.
//!
//! A worker that dies (a panic in a runtime) is seen by [`Worker::ended`]; the session
//! then ends the agent, because jobs nobody supervises must not go on being listed. Two
//! signals back the end mark up: a reply the worker's queue no longer has anybody to take
//! ([`Delivery::Gone`]), and a worker that has been inside one order or step for longer than
//! [`stall_bound`] on the lifecycle's clock ([`Worker::stalled`]).
//!
//! # Leaving with jobs held
//!
//! The thread is never joined, so a worker that is stuck cannot keep the process from
//! ending. An agent that ends for a reason other than a stop request must still not leave a
//! container with nobody to enforce its wall limit:
//!
//! - After a refusal for good the worker is alive: [`Worker::wind_down`] closes it, asks it
//!   to stop every run and waits until none is running or the worker has ended, for at
//!   most the stop grace plus [`WIND_DOWN_MARGIN`] (and never past the [`Rescue`] bound).
//!   What is still running then is stopped by the [`Rescue`] below. The results of the
//!   stopped runs are not sent: the server no longer takes them, and they may be lost.
//! - After the worker's end the worker cannot be asked. [`Worker::rescue`] closes the
//!   worker first (a worker that wakes from a stall must not start the runs still queued),
//!   makes a runtime of its own from the machine, lists the containers by the agent's
//!   label, and stops the ones that are running (at most [`RESCUE_MAX`], each call bounded
//!   by the runner's [`CALL_LIMIT`] and the stop grace, the whole by the [`Rescue`] bound,
//!   [`RESCUE_BOUND`] unless the stop grace is longer, on a thread that is left behind
//!   when the bound passes). A container that cannot be read is still stopped, and each one
//!   that could not be read or stopped is reported as a line. What is left is the
//!   supervisor's: it restarts the agent, whose next process adopts the containers that
//!   still exist and enforces their limits. The supervisor must restart the agent after
//!   every exit, status 0 and 2 included (systemd: `Restart=always`, no
//!   `RestartPreventExitStatus=2`). So the agent's own time to leave is the wind-down wait
//!   plus the rescue bound, minutes and not hours.
//!
//! A start the worker is already inside when it is closed is not undone: its container may
//! be made after the rescue listed, and then it is the next process's to adopt.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use js_agent_wire::{Command, HeartbeatReply, HeldRun, HeldState, RunResult, Taint};

use crate::cli_runtime::CALL_LIMIT;
use crate::facts::Platform;
use crate::integrity::Integrity;
use crate::lifecycle::{CheckHook, Config, Event, Lifecycle, Status};
use crate::readiness::{HookWindow, WorkerHook};

mod quiesce;

use crate::native::NativeChecks;
use crate::system::{Backend, Machine, RuntimeBuild, SharedClock};
use crate::usage::{Collector, Outbox};
use quiesce::take_asked;
pub use quiesce::{LeaseAnswers, WorkerHost};

/// How many orders wait for the worker. Replies beyond it are dropped, see the module.
pub const QUEUE: usize = 16;

/// How long the worker waits for an order before it steps the lifecycle by itself.
pub const STEP_EVERY: Duration = Duration::from_secs(1);

/// The time a worker may be inside one order or step that must make `calls` calls to the
/// runtime before it counts as stalled: each call may take [`CALL_LIMIT`] and, for a stop,
/// the stop grace on top, and [`STALL_MARGIN`] more is allowed for everything else. A drain
/// that stops many runs at once legitimately takes this long, so the bound grows with the
/// calls and the grace; `calls` is held to between 1 and [`STALL_CALLS_MAX`], or a host with
/// thousands of runs would have no watchdog at all.
pub fn stall_bound(stop_grace: Duration, calls: usize) -> Duration {
    let each = CALL_LIMIT.saturating_add(stop_grace);
    let calls = u32::try_from(calls.clamp(1, STALL_CALLS_MAX)).unwrap_or(u32::MAX);
    each.saturating_mul(calls).saturating_add(STALL_MARGIN)
}

/// The calls a start is counted as: the create, and the egress check's `network inspect`
/// and `nft` before it (the other calls of a start are inside the margin, as before).
pub const CALLS_PER_START: usize = 3;

/// The most calls [`stall_bound`] allows time for.
pub const STALL_CALLS_MAX: usize = 64;

/// What is allowed on top of the calls of [`stall_bound`].
pub const STALL_MARGIN: Duration = Duration::from_secs(60);

/// The most log lines kept for the session thread to print.
pub const LINES_CAP: usize = 256;

/// What the worker is made from. Everything in it is handed to the worker thread.
pub struct Plan {
    /// How the lifecycle is set up.
    pub config: Config,
    /// What the jobs run with, checked.
    pub backend: Backend,
    /// The id the agent registered under.
    pub host_id: String,
    /// The platform the agent runs on.
    pub platform: Platform,
    /// The directories the post-job checks watch.
    pub watch: Vec<PathBuf>,
    /// The clock, the runtime and the host view.
    pub machine: Arc<dyn Machine>,
    /// How long the worker waits for an order before it steps by itself.
    pub step_every: Duration,
    /// Where the usage of the leases is published for the session to take. Without it, or
    /// without `--actions-cgroup` in the settings, nothing is sampled.
    pub usage: Option<Arc<Outbox>>,
    /// The bound of the [`Rescue`] in place of [`RESCUE_BOUND`]: for a test.
    pub rescue_bound: Option<Duration>,
    /// The readiness hook the worker runs for a whole-node lease whose host it found quiet;
    /// none when the operator gave no `--readiness-hook`.
    pub readiness: Option<WorkerHook>,
}

/// The most containers [`Rescue::stop_all`] stops; the rest are reported as not tried. It
/// is far above what a host runs at once, so every container the agent started is stopped,
/// and it keeps the number of calls bounded.
pub const RESCUE_MAX: usize = 256;

/// How long a [`Rescue`] may take, unless the stop grace is longer (then the grace plus
/// [`WIND_DOWN_MARGIN`]). The stops are made one after the other, so with many containers
/// the bound can pass first; what is left is adopted by the next process, which the
/// supervisor starts.
pub const RESCUE_BOUND: Duration = Duration::from_secs(600);

/// What [`Worker::wind_down`] allows on top of the stop grace for the worker to stop the
/// runs itself.
pub const WIND_DOWN_MARGIN: Duration = Duration::from_secs(60);

/// How the agent stops its containers without the worker: a runtime of its own, made by
/// the machine the worker was made from.
#[derive(Clone)]
pub struct Rescue {
    machine: Arc<dyn Machine>,
    backend: Backend,
    host_id: String,
    platform: Platform,
    stop_grace: Duration,
    max: usize,
    bound: Duration,
    wind: Duration,
}

impl Rescue {
    /// A rescue over `machine` for the agent `host_id`, bounded by [`RESCUE_BOUND`] (or the
    /// stop grace and [`WIND_DOWN_MARGIN`], when that is longer) and stopping at most
    /// [`RESCUE_MAX`] containers. [`Worker::wind_down`] waits for the worker for the stop
    /// grace and [`WIND_DOWN_MARGIN`].
    pub fn new(
        machine: Arc<dyn Machine>,
        backend: Backend,
        host_id: String,
        platform: Platform,
        stop_grace: Duration,
    ) -> Self {
        Self {
            machine,
            backend,
            host_id,
            platform,
            stop_grace,
            max: RESCUE_MAX,
            bound: RESCUE_BOUND.max(stop_grace.saturating_add(WIND_DOWN_MARGIN)),
            wind: stop_grace.saturating_add(WIND_DOWN_MARGIN),
        }
    }

    /// The same, with the number of stops and the time they may take given: for a test.
    #[must_use]
    pub fn limited(mut self, max: usize, bound: Duration) -> Self {
        self.max = max;
        self.bound = bound;
        self
    }

    /// The same, with the time [`Worker::wind_down`] waits for the worker given: for a test.
    #[must_use]
    pub fn winding(mut self, wait: Duration) -> Self {
        self.wind = wait;
        self
    }

    /// Stops every container the agent labelled, on a runtime of its own. One line for each
    /// thing that did not work. Blocks for as long as the runtime does; see [`Rescue::run`].
    fn stop_all(&self) -> Vec<String> {
        let wanted = RuntimeBuild {
            backend: &self.backend,
            host_id: &self.host_id,
            platform: &self.platform,
            stop_grace: self.stop_grace,
        };
        let mut runtime = match self.machine.runtime(&wanted) {
            Ok((runtime, _)) => runtime,
            Err(why) => return vec![format!("rescue: no runtime: {why}")],
        };
        // The runs a native runtime cannot list are ended from its journal.
        let reclaimed: Vec<String> = runtime
            .reclaim()
            .into_iter()
            .map(|line| format!("rescue: {line}"))
            .collect();
        let ids = match runtime.list_ids() {
            Ok(ids) => ids,
            Err(why) => {
                let mut lines = reclaimed;
                lines.push(format!("rescue: cannot list the containers: {why}"));
                return lines;
            }
        };
        // Exited containers (the listing holds them too) must not use up the limit. One
        // that cannot be read is still tried: it may be running.
        let mut unread = Vec::new();
        let mut found = Vec::new();
        for id in ids {
            match runtime.poll(&id) {
                Ok(Status::Exited(_)) => {}
                Ok(_) => found.push(id),
                Err(why) => {
                    unread.push(format!(
                        "rescue: cannot read {}: {why}; stopping it anyway",
                        id.0
                    ));
                    found.push(id);
                }
            }
        }
        let stopped = found.iter().take(self.max).filter_map(|id| {
            let done = runtime.stop(id, self.stop_grace);
            done.err()
                .map(|why| format!("rescue: cannot stop {}: {why}", id.0))
        });
        let stopped: Vec<String> = stopped.collect();
        let skipped = found
            .iter()
            .skip(self.max)
            .map(|id| format!("rescue: {} not tried: too many containers", id.0));
        reclaimed
            .into_iter()
            .chain(unread)
            .chain(stopped)
            .chain(skipped)
            .collect()
    }

    /// [`Rescue::stop_all`] on a thread of its own, waited for at most the bound. A thread
    /// that is still inside the runtime then is left behind; the process ends anyway.
    fn run(&self) -> Vec<String> {
        let (done, lines) = sync_channel(1);
        let own = self.clone();
        thread::spawn(move || {
            let _ = done.send(own.stop_all());
        });
        lines.recv_timeout(self.bound).unwrap_or_else(|_| {
            vec!["rescue: did not finish in time; the next process adopts what is left".to_owned()]
        })
    }
}

/// What the next heartbeat carries of the jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every run the agent holds.
    pub held: Vec<HeldRun>,
    /// The results the server has not acknowledged.
    pub results: Vec<RunResult>,
    /// The taint to report, until the server acknowledges it.
    pub taint: Option<Taint>,
}

impl Snapshot {
    fn of(life: &Lifecycle) -> Self {
        Self {
            held: life.held(),
            results: life.results(),
            taint: life.pending_taint(),
        }
    }

    /// Whether a run is in the running state.
    pub fn running(&self) -> bool {
        let running = |run: &HeldRun| run.state == HeldState::Running;
        self.held.iter().any(running)
    }

    /// Whether the agent holds nothing and owes nothing.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty() && self.results.is_empty()
    }
}

/// What the session thread asks of the worker.
enum Order {
    /// A heartbeat reply, and when it arrived on the lifecycle's clock.
    Reply {
        reply: Box<HeartbeatReply>,
        received_ms: u64,
    },
    /// Look at the flags and step: there is no other work.
    Nudge,
    /// Answer on `0` once every order before this one is done and one step more is.
    Settle(SyncSender<()>),
    /// Find out whether the host is quiet for the lease asked about last (the order names
    /// none: see `Shared::asked_prepare`), run the readiness hook if there is one, and
    /// publish the answer.
    Prepare,
    /// Run the clean-up check of the lease asked about last (`Shared::asked_restore`), and
    /// publish the answer.
    Restore,
}

/// The worker's side of the lock: what it published.
#[derive(Default)]
struct Published {
    snapshot: Snapshot,
    answers: LeaseAnswers,
    lines: VecDeque<String>,
    lost: u64,
}

/// Everything both threads see.
#[derive(Default)]
struct Shared {
    published: Mutex<Published>,
    changed: Condvar,
    /// The agent takes no new job: a `Start` is not acted on.
    closed: AtomicBool,
    /// Stop every run the agent holds, once.
    stop_all: AtomicBool,
    /// A prepare order waits in the queue: set by [`WorkerHost`] when it sends one, cleared
    /// by the worker when it takes the order, so questions never crowd replies out.
    prepare_queued: AtomicBool,
    /// The same for a restore order.
    restore_queued: AtomicBool,
    /// The lease the latest prepare question was about. The order carries none, so the one
    /// that waits in the queue is always about the lease asked most recently.
    asked_prepare: Mutex<Option<String>>,
    /// The same for restore.
    asked_restore: Mutex<Option<String>>,
    /// The worker thread has ended: it returned, or it panicked.
    ended: AtomicBool,
    /// When the order or step the worker is inside is due to be done, on the lifecycle's
    /// clock ([`stall_bound`] after it began), plus one; 0 while it waits for an order.
    busy_until: AtomicU64,
}

/// Marks the worker as ended when the thread leaves, however it leaves. It is dropped
/// before the thread's channel is, so a caller that finds its order unanswered because the
/// thread is gone finds the mark set.
struct Ending<'a>(&'a Shared);

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        self.0.ended.store(true, Ordering::SeqCst);
        // Taking the lock between the mark and the wake-up keeps a waiter that has just
        // read the mark as unset from missing the wake-up.
        drop(self.0.lock());
        self.0.changed.notify_all();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Published> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, snapshot: Snapshot, answers: &LeaseAnswers, lines: Vec<String>) {
        let mut published = self.lock();
        published.snapshot = snapshot;
        published.answers = answers.clone();
        for line in lines {
            match published.lines.len() < LINES_CAP {
                true => published.lines.push_back(line),
                false => published.lost += 1,
            }
        }
        drop(published);
        self.changed.notify_all();
    }
}

/// What became of a reply handed to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The reply waits in the queue.
    Delivered,
    /// The worker is busy and the queue is full: the reply was dropped.
    Full,
    /// Nobody reads the queue any more: the worker's thread is gone.
    Gone,
}

/// A way to talk to the worker that can be copied: for a test to wait for it.
#[derive(Clone)]
pub struct Handle {
    orders: SyncSender<Order>,
    shared: Arc<Shared>,
}

impl Handle {
    /// Waits until the worker has done every order sent before this call and one step
    /// more, and published. False when the worker ended first.
    pub fn settle(&self) -> bool {
        let (done, finished) = sync_channel(1);
        // A worker that is gone drops the order, and with it `done`.
        let _ = self.orders.send(Order::Settle(done));
        finished.recv().is_ok()
    }

    /// Waits until `ready` holds of the published snapshot, for at most `guard`: a guard
    /// against a hang, not a time the caller expects. False when the guard ran out.
    pub fn wait_until(&self, ready: &dyn Fn(&Snapshot) -> bool, guard: Duration) -> bool {
        let published = self.shared.lock();
        let waited = self
            .shared
            .changed
            .wait_timeout_while(published, guard, |published| !ready(&published.snapshot))
            .unwrap_or_else(PoisonError::into_inner);
        !waited.1.timed_out()
    }

    /// What the next heartbeat would carry.
    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().snapshot.clone()
    }

    /// Whether the worker thread has ended.
    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::SeqCst)
    }
}

/// The session thread's end of the worker.
pub struct Worker {
    handle: Handle,
    clock: SharedClock,
    rescue: Rescue,
}

impl Worker {
    /// Starts the worker thread, builds the lifecycle on it from `plan` and adopts the
    /// containers the previous process left. Returns when that is done.
    ///
    /// # Errors
    ///
    /// The runtime or the checks cannot be made, the runtime cannot list its containers
    /// (the agent must not say hello with a held list it does not know), or the thread
    /// died while it built them. The text says why.
    pub fn start(mut plan: Plan) -> Result<Self, String> {
        let (orders, inbox) = sync_channel(QUEUE);
        let (ready, built) = sync_channel(1);
        let shared = Arc::new(Shared::default());
        let clock = plan.machine.clock();
        let hook = plan.readiness.take();
        let (own, step_every, thread_clock) = (shared.clone(), plan.step_every, clock.clone());
        let stop_grace = plan.config.stop_grace;
        let rescue = Rescue::new(
            plan.machine.clone(),
            plan.backend.clone(),
            plan.host_id.clone(),
            plan.platform.clone(),
            stop_grace,
        );
        let rescue = plan
            .rescue_bound
            .into_iter()
            .fold(rescue, |rescue, bound| rescue.limited(RESCUE_MAX, bound));
        // The thread is not joined, so a stuck worker cannot keep the process from ending;
        // the agent stops the containers it still holds itself (see the module) and a
        // container that is still there is adopted by the next process.
        thread::spawn(move || {
            let _ending = Ending(&own);
            match build(plan, thread_clock.clone()) {
                Err(why) => {
                    let _ = ready.send(Err(why));
                }
                Ok((life, usage, lines)) => {
                    own.publish(Snapshot::of(&life), &LeaseAnswers::default(), lines);
                    let _ = ready.send(Ok(()));
                    let seams = (step_every, stop_grace);
                    work(life, usage, hook, (&inbox, &own, &thread_clock), seams);
                }
            }
        });
        let outcome = match built.recv() {
            Ok(outcome) => outcome,
            Err(_) => Err("the job worker stopped while it started".to_owned()),
        };
        outcome.map(|()| Self {
            handle: Handle { orders, shared },
            clock,
            rescue,
        })
    }

    /// The lifecycle's clock, which a session that follows a whole-node lease counts its
    /// deadline on.
    pub fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    /// A copy of the handle, for waiting on the worker.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// What the next heartbeat carries.
    pub fn snapshot(&self) -> Snapshot {
        self.handle.snapshot()
    }

    /// A worker whose thread is gone and left no end mark, which only a fault in the
    /// thread's exit could leave: for the tests of the signals that do not rely on the
    /// mark. Its queue has no reader, so every reply is [`Delivery::Gone`]; it publishes
    /// `snapshot` and no more.
    #[doc(hidden)]
    pub fn orphaned(clock: SharedClock, snapshot: Snapshot, rescue: Rescue) -> Self {
        let (orders, _) = sync_channel(QUEUE);
        let published = Published {
            snapshot,
            ..Published::default()
        };
        let shared = Shared {
            published: Mutex::new(published),
            ..Shared::default()
        };
        Self {
            handle: Handle {
                orders,
                shared: Arc::new(shared),
            },
            clock,
            rescue,
        }
    }

    /// Hands a reply to the worker without waiting: [`Delivery::Full`] when the worker is
    /// busy and the reply was dropped, [`Delivery::Gone`] when nobody reads the queue. The
    /// reply is stamped with the clock now, so a start that waits in the queue has its
    /// `valid_for_ms` counted from the moment it arrived.
    pub fn try_deliver(&self, reply: HeartbeatReply) -> Delivery {
        let received_ms = self.clock.now_ms();
        let reply = Box::new(reply);
        let order = Order::Reply { reply, received_ms };
        match self.handle.orders.try_send(order) {
            Ok(()) => Delivery::Delivered,
            Err(TrySendError::Full(_)) => Delivery::Full,
            Err(TrySendError::Disconnected(_)) => Delivery::Gone,
        }
    }

    /// From now on a `Start` is not acted on. Stops, cancels and acknowledgements still
    /// are.
    pub fn close(&self) {
        self.handle.shared.closed.store(true, Ordering::SeqCst);
        let _ = self.handle.orders.try_send(Order::Nudge);
    }

    /// Stops every run the agent holds, once. Each result is sent and acknowledged as
    /// for any stop.
    pub fn stop_all(&self) {
        self.handle.shared.stop_all.store(true, Ordering::SeqCst);
        let _ = self.handle.orders.try_send(Order::Nudge);
    }

    /// The end after a refusal for good: closes the worker, asks it to stop every run and
    /// waits until none is running or the worker has ended, for at most the stop grace and
    /// [`WIND_DOWN_MARGIN`] (and the [`Rescue`] bound, when that is shorter); a worker that
    /// ended, or runs that still run then, are stopped by [`Worker::rescue`]. The lines say
    /// what could not be stopped.
    pub fn wind_down(&self) -> Vec<String> {
        self.close();
        self.stop_all();
        let shared = &self.handle.shared;
        let published = shared.lock();
        let waiting = |published: &mut Published| {
            published.snapshot.running() && !shared.ended.load(Ordering::SeqCst)
        };
        let wait = self.rescue.wind.min(self.rescue.bound);
        let _ = shared
            .changed
            .wait_timeout_while(published, wait, waiting)
            .unwrap_or_else(PoisonError::into_inner);
        // A worker that ended may have been inside a start: what it made is not in the
        // snapshot.
        match self.snapshot().running() || self.ended() {
            true => self.rescue(),
            false => Vec::new(),
        }
    }

    /// Closes the worker, so one that wakes from a stall takes no start that still waits in
    /// its queue, and stops every container of the agent without it, see the module. One
    /// line for each container that could not be stopped.
    pub fn rescue(&self) -> Vec<String> {
        self.close();
        self.rescue.run()
    }

    /// The lines the worker wants printed since the last call; a first line says how many
    /// were dropped when more than [`LINES_CAP`] piled up.
    pub fn take_lines(&self) -> Vec<String> {
        let mut published = self.handle.shared.lock();
        let mut lines: Vec<String> = published.lines.drain(..).collect();
        if published.lost > 0 {
            lines.insert(0, format!("{} log line(s) were dropped", published.lost));
            published.lost = 0;
        }
        lines
    }

    /// Whether the worker thread has ended.
    pub fn ended(&self) -> bool {
        self.handle.ended()
    }

    /// Whether the worker has been inside one order or step for longer than its
    /// [`stall_bound`] on the lifecycle's clock. A worker that waits for an order is never
    /// stalled.
    pub fn stalled(&self) -> bool {
        match self.handle.shared.busy_until.load(Ordering::SeqCst) {
            0 => false,
            due => self.clock.now_ms() > due - 1,
        }
    }
}

/// What `build` makes: the lifecycle, the usage collector when there is one, and the lines
/// to print.
type Built = (Lifecycle, Option<Collector>, Vec<String>);

/// The lifecycle, built and with the previous process's containers adopted, the collector of
/// the usage, and the lines to print about them.
fn build(plan: Plan, clock: SharedClock) -> Result<Built, String> {
    let Plan {
        config,
        backend,
        host_id,
        platform,
        watch,
        machine,
        usage,
        ..
    } = plan;
    let stop_grace = config.stop_grace;
    let wanted = RuntimeBuild {
        backend: &backend,
        host_id: &host_id,
        platform: &platform,
        stop_grace,
    };
    let (mut runtime, mut lines) = machine.runtime(&wanted)?;
    // What an earlier process left that no list of the runtime's can say (a native run).
    lines.extend(runtime.reclaim());
    // At start only: the rescue at the agent's end builds its runtime by `machine.runtime`
    // alone, so a firewall that has gone never keeps it from stopping the containers.
    runtime.check_egress().map_err(|why| why.to_string())?;
    let (checks, actions): (Box<dyn CheckHook>, Result<String, &str>) = match &backend {
        Backend::Container(settings) => {
            let integrity = Integrity::start(machine.view(), settings.uid(), watch)?;
            // The engines give each slot a user of its own; every crun job runs as the one id
            // of the job user's range, so there are no slots to tell apart.
            let integrity = match settings.crun() {
                Some(_) => integrity,
                None => integrity.with_slots(settings.slot_ids(0).0),
            };
            let actions = settings.actions_cgroup().map(str::to_owned);
            (Box::new(integrity), actions.ok_or(NO_ACTIONS_CGROUP))
        }
        Backend::Native(_) => (Box::new(NativeChecks), Err(NO_NATIVE_USAGE)),
    };
    let (life, events) = Lifecycle::adopt(config, runtime, checks, Box::new(clock))
        .map_err(|why| format!("cannot list the containers of an earlier run: {why}"))?;
    lines.extend(describe(&events));
    let collector = usage.and_then(|outbox| match actions {
        Ok(dir) => Some(Collector::new(
            machine.cgroup_files(),
            &dir,
            machine.usage_clock(),
            outbox,
        )),
        Err(why) => {
            lines.push(why.to_owned());
            None
        }
    });
    Ok((life, collector, lines))
}

/// Why the heartbeat of a native agent carries no load.
const NO_NATIVE_USAGE: &str = "usage: native runs report no load yet: the heartbeat carries none";

/// Why the heartbeat carries no load.
const NO_ACTIONS_CGROUP: &str =
    "usage: --actions-cgroup is not given: the heartbeat carries no load";

/// The worker's loop: an order or a step interval, then a step, then a publication.
fn work(
    mut life: Lifecycle,
    mut usage: Option<Collector>,
    hook: Option<WorkerHook>,
    (inbox, shared, clock): (&Receiver<Order>, &Shared, &SharedClock),
    (step_every, stop_grace): (Duration, Duration),
) {
    let mut answers = LeaseAnswers::default();
    // A readiness hook that takes its whole time is not a stuck worker: while a hook runs
    // the watchdog's limit is its bound longer, and when the hook returns the order has its
    // plain limit again, moved later by the time the hook really took. (The worker is the
    // only writer of `busy_until` during an order.)
    let (plain, opened) = (Cell::new(0), Cell::new(0));
    let watch = |window: HookWindow| {
        let limit = shared.busy_until.load(Ordering::SeqCst);
        let now = clock.now_ms();
        let moved = window.moves(limit, plain.get(), opened.get(), now);
        shared.busy_until.store(moved, Ordering::SeqCst);
        opened.set(now);
    };
    loop {
        let mut lines = Vec::new();
        let mut settle = None;
        let order = inbox.recv_timeout(step_every);
        let asked = order.as_ref().map_or(0, |order| match order {
            Order::Reply { reply, .. } => reply.commands.len().saturating_mul(CALLS_PER_START),
            _ => 0,
        });
        // A run held may cost three calls (a stop, then an inspect, the read of its log
        // and a remove).
        let calls = life.held().len().saturating_mul(3).saturating_add(asked);
        let bound = stall_bound(stop_grace, calls);
        let bound = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        let due = clock.now_ms().saturating_add(bound).saturating_add(1);
        shared.busy_until.store(due, Ordering::SeqCst);
        plain.set(due);
        match order {
            Ok(Order::Reply { reply, received_ms }) => {
                let closed = shared.closed.load(Ordering::SeqCst);
                let (reply, skipped) = match closed {
                    true => without_starts(*reply),
                    false => (*reply, Vec::new()),
                };
                lines.extend(skipped);
                lines.extend(describe(&life.apply_at(&reply, received_ms)));
            }
            Ok(Order::Settle(done)) => settle = Some(done),
            Ok(Order::Prepare) => {
                shared.prepare_queued.store(false, Ordering::SeqCst);
                // A second order whose lease the first took does nothing.
                let asked = take_asked(&shared.asked_prepare);
                let mut events = Vec::new();
                asked.into_iter().for_each(|lease| {
                    answers.prepare(&mut life, hook.as_ref(), lease, &mut events, &watch);
                });
                lines.extend(describe(&events));
            }
            Ok(Order::Restore) => {
                shared.restore_queued.store(false, Ordering::SeqCst);
                // A second order whose lease the first took does nothing.
                let asked = take_asked(&shared.asked_restore);
                let mut events = Vec::new();
                asked.into_iter().for_each(|lease| {
                    answers.restore(&mut life, lease, &mut events);
                });
                lines.extend(describe(&events));
            }
            Ok(Order::Nudge) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if shared.stop_all.swap(false, Ordering::SeqCst) {
            lines.extend(describe(&life.apply(&stops(&life))));
        }
        lines.extend(describe(&life.step()));
        answers.follow(life.exclusive());
        let sampled = usage
            .as_mut()
            .filter(|collector| collector.due())
            .map(|collector| {
                let mut said = life.find_cgroups();
                said.extend(collector.run(&life.running()));
                said
            });
        lines.extend(sampled.into_iter().flatten());
        shared.publish(Snapshot::of(&life), &answers, lines);
        shared.busy_until.store(0, Ordering::SeqCst);
        settle.iter().for_each(|done| {
            let _ = done.send(());
        });
    }
}

/// A reply that stops every run `life` holds.
fn stops(life: &Lifecycle) -> HeartbeatReply {
    let commands = life
        .held()
        .into_iter()
        .map(|run| Command::Stop { run_id: run.run_id })
        .collect();
    HeartbeatReply {
        commands,
        ..HeartbeatReply::default()
    }
}

/// `reply` with each `Start` made an unknown command (so the places of the others, which
/// a refusal is logged by, stay), and a line for each.
fn without_starts(mut reply: HeartbeatReply) -> (HeartbeatReply, Vec<String>) {
    let mut lines = Vec::new();
    reply.commands = reply
        .commands
        .into_iter()
        .map(|command| match command {
            Command::Start(spec) => {
                lines.push(format!(
                    "draining: the start of {} is not taken",
                    spec.run_id
                ));
                Command::Unknown
            }
            other => other,
        })
        .collect();
    (reply, lines)
}

/// One line for each event. The text stays on the host (a finding holds what the engine
/// said), and no event holds a token, a session secret or a job's labels.
fn describe(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| format!("job: {event:?}"))
        .collect()
}
