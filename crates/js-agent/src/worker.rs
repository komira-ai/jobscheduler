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
//! - **A whole-node lease is prepared on the worker's thread.** The post-job checks and the
//!   readiness hook block, so the session asks through [`WorkerHost`]: it sends a prepare or
//!   restore order without waiting and reads the last answer the worker published, which
//!   is tagged with the lease it is about. An answer for another lease is never handed
//!   back; the worker drops its answers when its lifecycle follows a different lease. The
//!   hook runs here, once for each lease.
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use js_agent_wire::{Command, HeartbeatReply, HeldRun, HeldState, RunResult, Taint};

use crate::cli_runtime::{CALL_LIMIT, Settings};
use crate::exclusive::Quiesce;
use crate::facts::Platform;
use crate::integrity::Integrity;
use crate::lifecycle::{Config, Event, Lifecycle, Quiet, Restored, Status};
use crate::readiness::WorkerHook;
use crate::system::{Machine, RuntimeBuild, SharedClock};

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
    /// The checked container flags.
    pub settings: Settings,
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
    settings: Settings,
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
        settings: Settings,
        host_id: String,
        platform: Platform,
        stop_grace: Duration,
    ) -> Self {
        Self {
            machine,
            settings,
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
            settings: &self.settings,
            host_id: &self.host_id,
            platform: &self.platform,
            stop_grace: self.stop_grace,
        };
        let mut runtime = match self.machine.runtime(&wanted) {
            Ok((runtime, _)) => runtime,
            Err(why) => return vec![format!("rescue: no runtime: {why}")],
        };
        let ids = match runtime.list_ids() {
            Ok(ids) => ids,
            Err(why) => return vec![format!("rescue: cannot list the containers: {why}")],
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
        unread.into_iter().chain(stopped).chain(skipped).collect()
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
    /// Find out whether the host is quiet for `lease` (and run the readiness hook if there
    /// is one), and publish the answer.
    Prepare { lease: String },
    /// Run the clean-up check of the lease that ended, and publish the answer.
    Restore { lease: String },
}

/// What the worker found about a whole-node lease, each answer tagged with the lease it is
/// about. The worker keeps the last of each.
#[derive(Debug, Clone, Default)]
struct Answers {
    quiet: Option<(String, Quiet)>,
    restored: Option<(String, Restored)>,
}

impl Answers {
    /// Whether the host is quiet for `lease`. An answer that is final (not busy) for the
    /// lease is kept, so the hook runs once for a lease however often it is asked.
    fn prepare(
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
    fn restore(&mut self, life: &mut Lifecycle, lease: String, events: &mut Vec<Event>) {
        let done = matches!(&self.restored, Some((held, Restored::Done)) if *held == lease);
        if done {
            return;
        }
        let answer = match life.exclusive() {
            Some(_) => Restored::Busy,
            None => life.restore(events),
        };
        self.restored = Some((lease, answer));
    }

    /// Drops what the lifecycle's lease (`held`) makes stale: a quiet answer about another
    /// lease, and every restored answer once a lease is followed again.
    fn follow(&mut self, held: Option<&str>) {
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

/// The worker's side of the lock: what it published.
#[derive(Default)]
struct Published {
    snapshot: Snapshot,
    answers: Answers,
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

    fn publish(&self, snapshot: Snapshot, answers: &Answers, lines: Vec<String>) {
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
            plan.settings.clone(),
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
                Ok((life, lines)) => {
                    own.publish(Snapshot::of(&life), &Answers::default(), lines);
                    let _ = ready.send(Ok(()));
                    let seams = (step_every, stop_grace);
                    work(life, hook, (&inbox, &own, &thread_clock), seams);
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

/// The lifecycle, built and with the previous process's containers adopted, and the lines
/// to print about it.
fn build(plan: Plan, clock: SharedClock) -> Result<(Lifecycle, Vec<String>), String> {
    let Plan {
        config,
        settings,
        host_id,
        platform,
        watch,
        machine,
        ..
    } = plan;
    let stop_grace = config.stop_grace;
    let wanted = RuntimeBuild {
        settings: &settings,
        host_id: &host_id,
        platform: &platform,
        stop_grace,
    };
    let (runtime, mut lines) = machine.runtime(&wanted)?;
    let checks = Integrity::start(machine.view(), settings.uid(), watch)?;
    let (life, events) = Lifecycle::adopt(config, runtime, Box::new(checks), Box::new(clock))
        .map_err(|why| format!("cannot list the containers of an earlier run: {why}"))?;
    lines.extend(describe(&events));
    Ok((life, lines))
}

/// The worker's loop: an order or a step interval, then a step, then a publication.
fn work(
    mut life: Lifecycle,
    hook: Option<WorkerHook>,
    (inbox, shared, clock): (&Receiver<Order>, &Shared, &SharedClock),
    (step_every, stop_grace): (Duration, Duration),
) {
    let mut answers = Answers::default();
    loop {
        let mut lines = Vec::new();
        let mut settle = None;
        let order = inbox.recv_timeout(step_every);
        let asked = order.as_ref().map_or(0, |order| match order {
            Order::Reply { reply, .. } => reply.commands.len(),
            _ => 0,
        });
        // A run held may cost two calls (a stop, then an inspect or a remove).
        let calls = life.held().len().saturating_mul(2).saturating_add(asked);
        // A readiness hook that takes its whole time is not a stuck worker.
        let hooked = match (&order, &hook) {
            (Ok(Order::Prepare { .. }), Some(hook)) => hook.bound(),
            _ => Duration::ZERO,
        };
        let bound = stall_bound(stop_grace, calls).saturating_add(hooked);
        let bound = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        let due = clock.now_ms().saturating_add(bound).saturating_add(1);
        shared.busy_until.store(due, Ordering::SeqCst);
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
            Ok(Order::Prepare { lease }) => {
                shared.prepare_queued.store(false, Ordering::SeqCst);
                let mut events = Vec::new();
                answers.prepare(&mut life, hook.as_ref(), lease, &mut events);
                lines.extend(describe(&events));
            }
            Ok(Order::Restore { lease }) => {
                shared.restore_queued.store(false, Ordering::SeqCst);
                let mut events = Vec::new();
                answers.restore(&mut life, lease, &mut events);
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
        shared.publish(Snapshot::of(&life), &answers, lines);
        shared.busy_until.store(0, Ordering::SeqCst);
        settle.iter().for_each(|done| {
            let _ = done.send(());
        });
    }
}

/// The session's way to ask the worker about a whole-node lease: a [`Quiesce`] host that
/// never blocks. Each question that the worker has not finally answered sends an order
/// without waiting (a full queue only means the question is asked again on the next
/// heartbeat) and the answer is what the worker last published, if that is about the lease
/// asked. Any other answer, or none yet, is busy, so an answer for one lease is never taken
/// for another. A final answer is not asked for twice.
pub struct WorkerHost {
    handle: Handle,
}

impl WorkerHost {
    /// A host that asks the worker `handle` belongs to.
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// Sends `order` unless a question of its kind already waits in the queue (the worker
    /// clears `queued` when it takes the order, so at most one of each kind is queued and
    /// questions never crowd replies out), or the queue has no room, which drops it.
    fn ask(&self, order: Order, queued: &AtomicBool) {
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
                let order = Order::Prepare {
                    lease: lease.to_owned(),
                };
                self.ask(order, &self.handle.shared.prepare_queued);
                Quiet::Busy
            }
        }
    }

    fn restore(&mut self, lease: &str) -> Restored {
        let published = self.handle.shared.lock().answers.restored.clone();
        match published {
            Some((held, Restored::Done)) if held == lease => Restored::Done,
            _ => {
                let order = Order::Restore {
                    lease: lease.to_owned(),
                };
                self.ask(order, &self.handle.shared.restore_queued);
                Restored::Busy
            }
        }
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
