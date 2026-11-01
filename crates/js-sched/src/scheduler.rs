//! The scheduler state machine.

mod commit;
mod fault;
mod give_up;
mod handoff;
mod hold;
mod ids;
pub(crate) mod loss;
mod machine;
pub(crate) mod memory;
mod outage;
mod phase;
mod recover;
mod snapshot;
mod state;
mod view;
mod whole;
mod whole_view;
mod worker;

pub use recover::{Fate, Recovered, RecoveredLease};
pub use snapshot::{
    Avoided, LeasePhase, LeaseView, MAX_AVOIDED, QueueGroup, QueueView, Snapshot, WaitingGroup,
    platform_label,
};
pub use state::OpState;
pub use whole::{WholeMark, WholeWait};
pub use whole_view::{WholeReason, WholeState, WholeSummary};
pub(crate) use worker::Worker;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use js_types::sched::usage::{CurveSource, LeaseCost, RunEnd};
use js_types::sched::{
    ControlRecord, Effect, Failure, FarmRun, FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId,
    Outcome, Qos, RefusalRecord, Resources, WaiterId, Waiting, WorkKey, WorkerId,
};

use js_whole::WholeNodeConfig;

use crate::admission::{Admission, Blocked, LoadConfig, Profile};
use crate::cordon::Cordons;
use crate::fence::HANDOVER_GRACE;
use crate::headroom::MemoryPercent;
use crate::input::Request;
use crate::requeue::{Requeue, RequeueReason};
use crate::servable::{Attempt, Placed, Servable, Verdict};
use crate::unservable::Unservable;
use memory::{MemoryLadder, MemoryState};

/// At most this many leases are granted per [`crate::Event::Tick`] (one log flush per round).
pub const PLACEMENT_ROUND: usize = 256;

/// How long a queued operation may wait while no live worker can run it (none
/// satisfies its platform, or none that does is large enough) before it is refused.
/// The default of [`Scheduler::new`]; see [`Scheduler::with_unservable_wait`].
///
/// The wait restarts whenever a live worker can run it again, and does not run while
/// only cordoned workers could (that work waits for the cordon). It is not zero because a
/// worker that can is often only a moment away: after a server restart daemons
/// reconnect over a few seconds, and a Mac that reboots is gone for a few minutes.
pub const UNSERVABLE_WAIT: Duration = Duration::from_secs(300);

/// How long a finished operation is kept after its waiters are answered, so that a
/// caller whose `Execute` stream broke can still `WaitExecution` on it and get its
/// result; after that the operation is gone. The default of
/// [`Scheduler::new`]; see [`Scheduler::with_finished_retention`].
///
/// A client reconnects after a broken stream within seconds (its retries back off to
/// a few seconds each), so a minute covers several attempts. The result it would
/// miss is worth keeping that long: a failed or `do_not_cache` result is not in the
/// action cache, so a client that finds the operation gone runs the action again. It
/// is not longer because every operation finished in the last retention is held in
/// memory: at the pilot's rate of about 4 operations a second, a minute is about 240.
pub const FINISHED_RETENTION: Duration = Duration::from_secs(60);

/// How a run that ends the operation ended: on its own, killed at its own memory limit or
/// by the node, or cut short (its result says nothing about how the action runs).
const fn run_end_of(outcome: Outcome) -> RunEnd {
    match outcome {
        Outcome::Completed { .. } => RunEnd::Clean,
        Outcome::Failed(Failure::OutOfMemory) => RunEnd::KilledAtLimit,
        Outcome::Failed(Failure::NodeMemoryPressure) => RunEnd::BackstopKilled,
        Outcome::Failed(Failure::Infra | Failure::Lost(_)) => RunEnd::Requeued,
        Outcome::Failed(Failure::Timeout | Failure::Invalid | Failure::MissingInput) => {
            RunEnd::Cancelled
        }
    }
}

#[derive(Clone, Debug)]
struct Operation {
    request: Request,
    waiters: Vec<WaiterId>,
    state: OpState,
    /// The lease of the newest committed grant, in log order. A committed result is
    /// accepted only from this lease.
    committed_lease: Option<LeaseId>,
    /// A result from the current holding has been proposed; later reports for it are
    /// duplicates.
    result_proposed: bool,
    /// The proposed result is the farm's give-up of a silent, replaced or never-started
    /// lease past its budget ([`Scheduler::give_up_charged`]), not a worker's.
    gave_up: bool,
    /// While queued: since when, and why, no live worker can run it. `None` while one
    /// can.
    unservable: Option<UnservableWait>,
    /// Its runs that ended as the farm's fault, oldest first: the farm reruns it has
    /// used ([`memory::FARM_RERUNS`]).
    farm_runs: Vec<FarmRun>,
    /// The workers those runs ended on: placement prefers any other (see
    /// [`Servable::fit`]).
    avoid: BTreeSet<WorkerId>,
    /// Its leases given up as not started ([`memory::NOT_STARTED_RERUNS`]).
    not_started: u32,
    /// Its leases lost ([`RequeueReason::is_loss`]), counted toward the cap
    /// ([`Scheduler::with_loss_requeues`]).
    losses: u32,
    /// When it last entered the queue: its submission, or the give-up of its lease
    /// ([`Scheduler::queue_snapshot`] ages the queue from it).
    queued_at: FarmTime,
    /// While its memory is one a key floor raised: what it booked before and the raise
    /// ([`Scheduler::reclamp_key_floor`] undoes the raise when no live worker is large
    /// enough for it).
    key_applied: Option<KeyApplied>,
}

/// A memory raise a key floor made to a queued operation.
#[derive(Clone, Copy, Debug)]
struct KeyApplied {
    /// The memory the operation booked before the first raise.
    base: u64,
    /// The memory the raise gave it.
    applied: u64,
    /// The raise was taken back because no worker could take it without owning the worker
    /// whole: the operation books what it asked for, and the floor does not raise it again.
    undone: bool,
}

/// A queued operation no live worker can run: since when, why, and whether the wait
/// counts toward its refusal.
#[derive(Clone, Debug)]
struct UnservableWait {
    since: FarmTime,
    reason: Unservable,
    /// `reason` as callers are told it, made once when the reason changes.
    text: String,
    /// `false` while only cordoned workers could run it: it waits for the cordon to
    /// end and is never refused for it.
    refusable: bool,
}

/// A lease currently held (leased or running).
#[derive(Clone, Debug)]
struct Held {
    operation: OperationId,
    /// What it booked on its worker.
    booked: Resources,
    /// What admission charged it: fixed at its placement, kept until its result.
    cost: LeaseCost,
    /// The iOS device it booked on its worker, by id.
    device: Option<String>,
    /// When and to which session its `Start` was emitted; `None` until its grant is
    /// committed.
    start_sent: Option<StartSent>,
    /// When the grant was made (proposed), and when the worker reported it started.
    granted_at: FarmTime,
    started_at: Option<FarmTime>,
}

/// When a `Start` was emitted, and the worker session and daemon process it was sent to.
#[derive(Clone, Copy, Debug)]
struct StartSent {
    at: FarmTime,
    session: u64,
    process: u64,
}

/// The scheduler core of one leader term.
///
/// It owns the queue, the operations, the workers' bookings and the leases, and decides
/// placements. It reads no clock: farm time arrives in every [`Input`]. Leases are
/// numbered `(term, seq)` from the term it was built with.
///
/// Two rules carry its safety, and each holds whatever order inputs arrive in:
///
/// - **Commit before Start.** Placing an operation emits only [`Effect::Commit`] of
///   the grant. The [`Effect::Start`] is emitted when that record comes back as
///   [`Event::Committed`], and never for a grant that is no longer current by then.
/// - **One accepted result per operation.** A report is proposed only from the lease
///   the operation currently holds under a committed grant, at most once per holding.
///   A committed result is accepted only if its lease is the operation's newest
///   committed grant (log order decides) and the operation is not finished. Results
///   from an expired lease, duplicates and late arrivals are dropped. A committed
///   memory kill or lost run that runs the operation again ([`memory`], [`fault`])
///   finishes nothing: the operation is queued again, and only a later run's result
///   can finish it.
///
/// A lease is given up, and its operation requeued, when its worker is silent for G,
/// and when a worker it still hears from does not list it as running (see
/// [`Event::WorkerUp`] and [`Event::Heartbeat`]): at once only if the daemon process
/// that received its `Start` says so, or was proven stopped by the process after it,
/// after the handover grace if another process registered as the worker since. A given-up lease can no longer have a
/// result proposed, and once the operation is granted again its result loses to the
/// new grant in the log.
#[derive(Clone, Debug)]
pub struct Scheduler {
    term: u64,
    next_seq: u64,
    /// No lease gets a sequence at or past this one ([`Scheduler::use_seq_block`]).
    seq_limit: u64,
    next_op: u64,
    now: FarmTime,
    ops: BTreeMap<OperationId, Operation>,
    /// Queued operations, most urgent first, then oldest first.
    queue: BTreeSet<(Reverse<Qos>, OperationId)>,
    /// Joinable operations not yet finished, by dedup key.
    in_flight: BTreeMap<WorkKey, OperationId>,
    workers: BTreeMap<WorkerId, Worker>,
    /// Every lease currently held (leased or running).
    held: BTreeMap<LeaseId, Held>,
    /// How long a queued operation no live worker can run waits before it is refused.
    unservable_wait: Duration,
    /// Workers placement skips, and their drains.
    cordons: Cordons,
    /// How long a finished operation is kept.
    finished_retention: Duration,
    /// Finished operations still kept, in the order they finished, with when they did
    /// (so in time order: farm time never goes back).
    finished: VecDeque<(FarmTime, OperationId)>,
    /// Requeues not yet taken, while they are recorded at all
    /// ([`Scheduler::recording_requeues`]).
    requeues: Option<Vec<Requeue>>,
    /// Queued whole-machine operations that fit nowhere, and the worker each holds:
    /// work after it in queue order is not placed there (see [`Scheduler::place`]).
    reservations: BTreeMap<OperationId, WorkerId>,
    /// The memory floors, each operation's memory kills and each node's busy-node
    /// kills ([`memory`]). Boxed: most schedulers hold none, and the scheduler is
    /// moved by value.
    memory: Box<MemoryState>,
    /// The size key of each operation the caller named one for ([`Scheduler::set_size_key`]),
    /// until the operation is retired. Not in the request, the log or the `Eq` memory state.
    size_keys: BTreeMap<OperationId, u64>,
    /// The rungs of an action's memory booking ([`memory`]).
    ladder: MemoryLadder,
    /// Whether an own-limit kill climbs the ladder ([`Scheduler::set_memory_climb`]).
    climb: bool,
    /// Workers a recovered scheduler holds leases for that have not registered yet, and
    /// until when they may take to ([`Scheduler::recover`]).
    awaited: BTreeMap<WorkerId, FarmTime>,
    /// Awaited workers whose recovered leases went to a daemon process that may not be the
    /// one that registers, and until when those leases are kept once it does
    /// ([`Scheduler::hold_recovered_leases`]).
    other_process: BTreeMap<WorkerId, FarmTime>,
    /// The state store refused a write and has not taken one since
    /// ([`Scheduler::store_unavailable`]).
    store_down: bool,
    /// Until when no lease is given up, after the store returned
    /// ([`Scheduler::store_available`]).
    grace_until: FarmTime,
    /// How many lost leases refuse an operation ([`Scheduler::with_loss_requeues`]).
    loss_cap: u32,
    /// The load predicate's settings, counters and cost predictions ([`crate::admission`]).
    admission: Admission,
    /// The share of a worker's reported memory that is its capacity here
    /// ([`Scheduler::with_memory_percent`]).
    memory_percent: MemoryPercent,
    /// The worker each blocked head of the queue holds (see [`Scheduler::place`]); the
    /// last round's, kept so the hold does not wander.
    head_holds: BTreeMap<OperationId, WorkerId>,
    /// The whole-node settings ([`Scheduler::with_whole_node`]); `None` holds a worker for
    /// a blocked whole-machine request by leases and booking alone.
    whole_node: Option<WholeNodeConfig>,
    /// The longest each operation the caller named one for may run
    /// ([`Scheduler::set_max_wall`]), until the operation is retired. Not in the request,
    /// the log or the `Eq` memory state.
    max_walls: BTreeMap<OperationId, Duration>,
    /// The marks, give-up counts and back-offs of whole-machine requests
    /// ([`Scheduler::whole_mark`]); empty without whole-node settings.
    drain: whole::Drain,
}

impl Scheduler {
    /// An empty scheduler for the leader of `term`.
    #[must_use]
    pub fn new(term: u64) -> Self {
        Self {
            term,
            next_seq: 0,
            seq_limit: u64::MAX,
            next_op: 0,
            now: FarmTime::default(),
            ops: BTreeMap::new(),
            queue: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            workers: BTreeMap::new(),
            held: BTreeMap::new(),
            unservable_wait: UNSERVABLE_WAIT,
            cordons: Cordons::default(),
            finished_retention: FINISHED_RETENTION,
            finished: VecDeque::new(),
            requeues: None,
            reservations: BTreeMap::new(),
            memory: Box::default(),
            size_keys: BTreeMap::new(),
            ladder: MemoryLadder::default(),
            climb: true,
            awaited: BTreeMap::new(),
            other_process: BTreeMap::new(),
            store_down: false,
            grace_until: FarmTime::default(),
            loss_cap: u32::MAX,
            admission: Admission::default(),
            memory_percent: MemoryPercent::FULL,
            head_holds: BTreeMap::new(),
            whole_node: None,
            max_walls: BTreeMap::new(),
            drain: whole::Drain::default(),
        }
    }

    /// This scheduler, keeping a finished operation for `retention` after its waiters
    /// are answered (instead of [`FINISHED_RETENTION`]), then dropping it.
    #[must_use]
    pub const fn with_finished_retention(mut self, retention: Duration) -> Self {
        self.finished_retention = retention;
        self
    }

    /// This scheduler, admitting actions as `config` says. A scheduler built with
    /// [`Scheduler::new`] places by load with the defaults of [`LoadConfig`]: `On`, 5 s
    /// windows, `f` 1.0, `s_cpu` 0.10, `s_mem` 0, the flat-memory guard at `d` 0 and charges
    /// ending at the expected life plus 2 minutes. `PlacementLoad::Off` here is the
    /// rollback switch to the booked rule.
    #[must_use]
    pub fn with_load_config(mut self, config: LoadConfig) -> Self {
        self.admission.configure(config);
        self
    }

    /// This scheduler, taking `percent` of the memory a worker reports as that worker's
    /// capacity: `floor(memory * percent / 100)` bytes, once, when the worker registers or
    /// its report changes. Every admission path reads the capacity so cut (the load
    /// predicate, the flat-memory guard, the memory ladder's cap, the whole-host test, VM
    /// booking, the unservable check) and [`Scheduler::capacity`] shows it. Cores, GPUs,
    /// VM slots and the lease cap are not cut, and neither is any figure an agent
    /// measures or a job's learned memory. Set it before the first worker registers.
    #[must_use]
    pub const fn with_memory_percent(mut self, percent: MemoryPercent) -> Self {
        self.memory_percent = percent;
        self
    }

    /// The share of a worker's reported memory this scheduler takes as its capacity.
    #[must_use]
    pub const fn memory_percent(&self) -> MemoryPercent {
        self.memory_percent
    }

    /// This scheduler, holding `curves` for the rule that charges a lease by a curve over
    /// its lifetime. The flat rule does not read them: a key that has a curve is charged
    /// flat and counted ([`LoadCounters::curve_flat`]).
    #[must_use]
    pub fn with_curves(mut self, curves: Arc<dyn CurveSource + Send + Sync>) -> Self {
        self.admission.use_curves(curves);
        self
    }

    /// This scheduler, holding the worker for a blocked whole-machine request that is the
    /// cheapest to drain ([`js_whole::cost::drain_cost`]: the latest end bound of its
    /// leases, then the fewest leases, then the first by name) instead of the one with the
    /// fewest leases and the least booked. An end bound is a lease's start plus the longest
    /// it may run ([`Scheduler::set_max_wall`]). Without this a scheduler holds by leases
    /// and booking alone.
    ///
    /// A worker that fits (one with no lease) is taken at once while the ceiling and the
    /// floor kept for ordinary work allow one more whole worker (`max_draining` does not
    /// apply: the take drains nothing, and the last-capable rule is not read: an older
    /// ordinary request a quiet worker could run is placed on it first). A request that
    /// holds a mark takes it whatever the caps, as its mark is already counted; any other
    /// that a cap stops stays queued and [`Scheduler::whole_wait`] says which. A request that fits
    /// nowhere holds a worker only after it has waited `drain_after` since it was queued,
    /// and marks it ([`Scheduler::whole_mark`]) with a deadline from
    /// [`js_whole::cost::reserve_deadline`] (the longest it may run is the one given with
    /// [`Scheduler::set_max_wall`]; with none, no deadline but `drain_deadline`). At the
    /// deadline, if the worker is still busy, the hold is given up: counted, dropped, the
    /// worker barred to the request for `reserve_backoff`, and another held if there is
    /// one. Nothing running is stopped. The marks, counts and back-offs are in memory
    /// only; a recovered scheduler has none, so every count restarts at zero.
    ///
    /// A new mark is made only while the caps allow it: whole workers (marks and held
    /// whole-machine leases, never a head's hold) below `max_whole_node` and below the
    /// capable workers less `keep_for_ordinary`, marks on busy workers below
    /// `max_draining`, and the worker not the last one an older ordinary request that
    /// waits could run on, unless `allow_last_capable` (then the next worker by drain cost
    /// is tried). A request a cap stops holds nothing and [`Scheduler::whole_wait`] says
    /// which. The pool is every worker that serves whole-machine leases: all but a
    /// quarantined or draining one counts as capable, a lost one too. When the ceiling falls
    /// below the whole workers, the newest marks beyond it are dropped (no give-up
    /// counted), never a held lease. `max_whole_node_per_consumer` is not enforced
    /// here: no request, waiter or qos names a consumer, so the setting is ignored. The
    /// lease server or the registry, where the caller's identity exists, enforces it.
    /// Workers of every platform count toward one ceiling and one `max_draining`.
    #[must_use]
    pub fn with_whole_node(mut self, config: WholeNodeConfig) -> Self {
        self.whole_node = Some(config);
        self
    }

    /// How many operations it holds: every unfinished one, and each finished one for
    /// the finished retention.
    #[must_use]
    pub fn operations(&self) -> usize {
        self.ops.len()
    }

    /// This scheduler, keeping a [`Requeue`] for each lease it gives up until
    /// [`Scheduler::take_requeues`] takes it. Off by default, so a caller that never
    /// takes them (a simulation) does not collect them without end.
    #[must_use]
    pub fn recording_requeues(mut self) -> Self {
        self.requeues = Some(Vec::new());
        self
    }

    /// The leases given up since the last call, oldest first; always empty unless
    /// built with [`Scheduler::recording_requeues`].
    pub fn take_requeues(&mut self) -> Vec<Requeue> {
        self.requeues
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// This scheduler, refusing a queued operation once no live worker has been able to
    /// run it for `wait` (instead of [`UNSERVABLE_WAIT`]).
    #[must_use]
    pub const fn with_unservable_wait(mut self, wait: Duration) -> Self {
        self.unservable_wait = wait;
        self
    }

    /// Drops every finished operation kept for the finished retention or longer. A
    /// finished operation holds no lease and is in no queue or in-flight entry, so
    /// nothing else names it; an input that does (a late report, a stale record) finds
    /// no operation and is dropped, as for any unknown one.
    fn retire(&mut self) {
        while let Some(&(at, id)) = self.finished.front()
            && self.now >= at.saturating_add(self.finished_retention)
        {
            self.finished.pop_front();
            self.ops.remove(&id);
            self.memory.forget(id);
            self.size_keys.remove(&id);
            self.max_walls.remove(&id);
            self.drain.forget(id);
            self.admission.forget(id);
        }
    }

    /// Queues `request` for `waiter`, or attaches `waiter` to a running twin. A twin
    /// that waits for a worker that can run it tells its waiters why again, so the new
    /// one learns it too.
    fn submit(&mut self, waiter: WaiterId, request: Request) -> Vec<Effect> {
        if request.joinable()
            && let Some(&id) = self.in_flight.get(&request.key)
        {
            let op = self
                .ops
                .get_mut(&id)
                .expect("in-flight entries name live operations");
            op.waiters.push(waiter);
            if request.qos > op.request.qos {
                if self.queue.remove(&(Reverse(op.request.qos.clone()), id)) {
                    self.queue.insert((Reverse(request.qos.clone()), id));
                }
                op.request.qos = request.qos;
            }
            return op
                .unservable
                .iter()
                .map(|u| {
                    Effect::Waiting(Waiting {
                        operation: id,
                        reason: Some(u.text.clone()),
                    })
                })
                .collect();
        }
        let id = OperationId(self.next_op);
        self.next_op += 1;
        let request = self.floored(request);
        if request.joinable() {
            self.in_flight.insert(request.key.clone(), id);
        }
        self.queue.insert((Reverse(request.qos.clone()), id));
        self.ops.insert(
            id,
            Operation {
                request,
                waiters: vec![waiter],
                state: OpState::Queued,
                committed_lease: None,
                result_proposed: false,
                gave_up: false,
                unservable: None,
                farm_runs: Vec::new(),
                not_started: 0,
                losses: 0,
                avoid: BTreeSet::new(),
                queued_at: self.now,
                key_applied: None,
            },
        );
        Vec::new()
    }

    /// Releases `operation`'s holding (booking and lease), if it has one. `end` is how its
    /// run ended, for the shape guard ([`Admission::run_ended`]).
    fn release(&mut self, id: OperationId, end: RunEnd) {
        let op = self.ops.get_mut(&id).expect("released operations exist");
        if let Some((lease, worker)) = op.state.holding() {
            let held = self.held.remove(&lease);
            if let Some((w, held)) = self.workers.get_mut(worker).zip(held) {
                w.booked = w.booked.saturating_sub(held.booked);
                if let Some(ended) = w.load_state.remove(lease) {
                    self.admission.run_ended(&ended, end);
                }
                w.leases -= 1;
                if let Some(device) = &held.device {
                    w.devices.remove(device);
                }
                // A whole-machine lease, or an action at the whole host's memory, is
                // the node's only lease.
                if w.leases == 0 {
                    w.whole = false;
                }
            }
        }
        op.result_proposed = false;
        op.gave_up = false;
    }

    /// Releases `operation`'s holding and puts it back in the queue, recording why.
    fn requeue(&mut self, id: OperationId, reason: RequeueReason) {
        let given_up = self.ops[&id]
            .state
            .holding()
            .map(|(lease, worker)| Requeue {
                operation: id,
                lease,
                worker: worker.clone(),
                reason,
            });
        if let Some(log) = &mut self.requeues {
            log.extend(given_up);
        }
        self.release(id, reason.run_end());
        let op = self
            .ops
            .get_mut(&id)
            .expect("held leases name live operations");
        // Only an operation that holds a lease is requeued, so every call is a give-up.
        op.losses += u32::from(reason.is_loss());
        op.state = OpState::Queued;
        op.queued_at = self.now;
        self.queue.insert((Reverse(op.request.qos.clone()), id));
    }

    /// One placement round. Each queued operation, most urgent first, goes to the least
    /// loaded live worker that serves its lease kind, satisfies its platform and has room
    /// for it, up to [`PLACEMENT_ROUND`] grants: the one with the least booked, as a
    /// share of its capacity (the larger of its CPU and memory shares), then the fewest
    /// leases, then the first by name ([`Worker::load`]), so work spreads over the farm
    /// instead of filling one worker after another. An action has room where its CPUs,
    /// GPUs and VM slots are free and no whole-machine lease is held; its memory is only
    /// checked against the worker's, never against what other leases hold
    /// ([`Worker::has_room`]), though what they hold ranks the workers; a VM has room
    /// only where its full request, memory included, fits what is free, so it can wait
    /// behind actions and nothing reserves a worker for it; a
    /// whole-machine lease only on a worker that holds no lease, and it books all of it.
    /// A round places no more leases than the block of sequences the scheduler was given
    /// has left ([`Scheduler::use_seq_block`]); the rest wait for the next round.
    ///
    /// Under admission by load (the default; [`Scheduler::with_load_config`]) an action has room where
    /// the load predicate admits it (see [`crate::LoadConfig`]), and workers are ranked by
    /// what they are charged, measured and predicted, instead of by what is booked. An
    /// action that has a worker with room but for the predicate is blocked; the first
    /// such action of a pool in queue order (the head) holds the worker of the pool with
    /// the smallest binding ratio, `max((L_cpu + p) / C_cpu, (L_mem + m) / C_mem)`, and
    /// keeps it while its ratio is within a tenth of the best: no operation after it in
    /// queue order is placed there, so the worker empties as its leases end. At most one
    /// worker is held per pool, work before the head is never held back, and the head
    /// of a pool blocked only by the cold cap holds nothing. A worker held for a
    /// whole-machine operation or for a head, a draining, cordoned or whole-machine worker
    /// is out before the predicate is read.
    ///
    /// A whole-machine operation that fits nowhere holds one worker that could run it
    /// (the one it held last round while that still could, else the one with fewest
    /// leases): no operation after it in queue order (less urgent, or as urgent and
    /// younger) is placed there, so the worker empties as its leases end. Work before
    /// it in queue order still is, so a reservation never holds back more urgent work.
    /// It lasts while the operation is queued and the worker could run it. No other
    /// operation holds a worker: an action at a worker's whole memory (the last step of
    /// the memory ladder) waits until some worker has no lease, and under sustained
    /// spread load that wait can be long, since nothing reserves a worker for it.
    ///
    /// Every queued operation is also checked against what live workers could ever give
    /// it: one that no live worker satisfies, or that is larger than every one that
    /// does, waits with a reason ([`Effect::Waiting`] when the reason changes), and is
    /// refused once it has waited so for the unservable wait. Its refusal is committed
    /// first and leaves the queue at once, so nothing places it meanwhile.
    /// What operation `id`, asking `request`, is charged if placed now: its cost and how
    /// that is charged over time (`None`: flat).
    fn charging(&self, id: OperationId, request: &Request) -> (LeaseCost, Option<Arc<Profile>>) {
        let retried = !self.memory.runs(id).is_empty();
        let cost = self.admission.cost_of(id, request, retried);
        // The shape guard judges a key's runs together: the caller's size key, which the
        // curves are learned under, else the request's own key.
        let guard_key = self.size_keys.get(&id).copied().unwrap_or(cost.key_hash);
        let profile = self.admission.profile_of(id, request, &cost, guard_key);
        (cost, profile)
    }

    fn place(&mut self, effects: &mut Vec<Effect>) {
        // A grant the store cannot take is useless, and a refusal as much.
        if self.store_down {
            return;
        }
        self.refuse_lost(effects);
        // Before any operation is fitted: a floor raised since it was queued applies.
        self.apply_key_floors();
        let now = self.now;
        let left = usize::try_from(self.seqs_left()).unwrap_or(usize::MAX);
        let round = PLACEMENT_ROUND.min(left);
        let mut servable = Servable::new(&self.workers, &self.cordons, now);
        let mut placed = Vec::new();
        let mut verdicts = Vec::new();
        let mut stuck = Vec::new();
        let mut reserved = BTreeSet::new();
        let mut reservations = BTreeMap::new();
        let mut whole = self.whole_round();
        let mut head_holds = BTreeMap::new();
        let mut held_pools = BTreeSet::new();
        for (position, &(_, id)) in (0u64..).zip(&self.queue) {
            let op = &self.ops[&id];
            let request = &op.request;
            // A memory a key floor gave it never owns a worker whole while another could
            // take it.
            let key_raised = op
                .key_applied
                .is_some_and(|a| !a.undone && a.applied == request.resources.memory_bytes);
            // The lease it gets if it is placed now: leases are numbered in the order of
            // `placed` below.
            let lease = LeaseId::new(
                self.term,
                self.next_seq + u64::try_from(placed.len()).unwrap_or(u64::MAX),
            );
            let (cost, profile) = self.charging(id, request);
            let fit_minutes = self.admission.fit_minutes(id, profile.as_deref());
            // A whole-machine request avoids the workers it gave up for the back-off.
            let barred = self.drain.barred(id, now);
            let avoid = whole::Drain::avoiding(&op.avoid, &barred);
            let attempt = Attempt {
                request,
                reserved: &reserved,
                avoid: &avoid,
                not_whole: key_raised,
                cost,
                profile: profile.clone(),
                lease,
                fit_minutes,
            };
            let mut blocked = None;
            // A whole-machine request over a cap takes no quiet worker; it waits as any
            // request that cannot be placed does.
            let fitted = placed.len() < round
                && (request.kind != LeaseKind::WholeMachine || self.quiet_take(&whole, id))
                && {
                    let fit = servable.fit(&mut self.workers, &mut self.admission, &attempt);
                    blocked = fit.blocked;
                    fit.placed.map(|p| placed.push((id, p))).is_some()
                };
            let verdict = if fitted {
                Verdict::Servable
            } else {
                servable.verdict(&self.workers, request)
            };
            // A whole-machine request that is placed, or cannot be served, has no hold to
            // count; one that fits ahead of the rest counts as a whole worker.
            if request.kind == LeaseKind::WholeMachine && (fitted || verdict != Verdict::Servable) {
                self.drain.marks.remove(&id);
                if fitted {
                    whole.placed_whole();
                }
            } else if !fitted && request.kind != LeaseKind::WholeMachine {
                whole.waiting_ordinary(position, id);
            }
            // A whole-machine request holds a worker while it waits for it to empty, and so
            // does an action that owns its worker; the drain order of a configured
            // whole-node pool is for whole-machine requests alone.
            if !fitted
                && (request.kind == LeaseKind::WholeMachine || request.owns_worker())
                && verdict == Verdict::Servable
            {
                let held = match &self.whole_node {
                    Some(cfg) if request.kind == LeaseKind::WholeMachine => {
                        let turn = self.drain.turn(id, op.queued_at, now, cfg);
                        turn.and_then(|turn| {
                            match self.hold_whole(
                                &mut servable,
                                &whole,
                                (id, position),
                                &reserved,
                                &placed,
                                turn,
                            ) {
                                Ok(held) => held.map(|(name, mark)| {
                                    whole.marks.insert(id, mark);
                                    name
                                }),
                                Err(wait) => {
                                    whole.waits.insert(id, wait);
                                    None
                                }
                            }
                        })
                    }
                    // Without whole-node settings: as it always was, the worker with the
                    // fewest leases, kept while it could run the request, and no limit
                    // on how many requests hold a worker.
                    None => servable.reserve_by_leases(
                        &self.workers,
                        request,
                        &reserved,
                        &op.avoid,
                        self.reservations.get(&id),
                    ),
                    // With settings, an action that owns its worker holds one worker per
                    // pool, by the least tail, the first request in queue order taking it
                    // (a blocked head shares the pool's slot below).
                    Some(_) => {
                        let pool = servable.pool(&self.workers, request);
                        let drain = self.drain_order(&placed);
                        let open = !held_pools.contains(&pool);
                        let name = open
                            .then(|| {
                                servable.reserve(
                                    &self.workers,
                                    request,
                                    &reserved,
                                    &op.avoid,
                                    &drain,
                                )
                            })
                            .flatten();
                        if name.is_some() {
                            held_pools.insert(pool);
                        }
                        name
                    }
                };
                if let Some(name) = held {
                    reserved.insert(name.clone());
                    reservations.insert(id, name);
                }
            }
            // A blocked head holds the worker with the most headroom, one per pool:
            // requests after it in queue order are not placed there, so the worker
            // empties as its leases end instead of being refilled by small work. The
            // pool's slot is taken only by a request that holds a worker: one that
            // cannot (an exclusive action, say) leaves it to the next blocked request.
            let pool =
                (blocked == Some(Blocked::Load)).then(|| servable.pool(&self.workers, request));
            if let Some(pool) = pool
                && !held_pools.contains(&pool)
            {
                let attempt = Attempt {
                    request,
                    reserved: &reserved,
                    avoid: &op.avoid,
                    not_whole: key_raised,
                    cost,
                    profile,
                    lease,
                    fit_minutes,
                };
                let kept = self.head_holds.get(&id);
                // Without whole-node settings the hold is what it always was, by the
                // binding ratio; with them, by the start time.
                let held = match &self.whole_node {
                    None => servable.hold_for_head(&self.workers, &self.admission, &attempt, kept),
                    Some(_) => {
                        let ends = self.lease_ends(&placed);
                        servable.hold_for_head_by_start(
                            &self.workers,
                            &self.admission,
                            &attempt,
                            &ends,
                        )
                    }
                };
                if let Some(name) = held {
                    self.admission.counters.reservations += u64::from(kept != Some(&name));
                    reserved.insert(name.clone());
                    head_holds.insert(id, name);
                    held_pools.insert(pool);
                }
            }
            if !fitted
                && key_raised
                && verdict == Verdict::Servable
                && !servable.roomy(&self.workers, request)
            {
                // Only a worker the raise would own whole is left: it books what it
                // asked for before the floor instead.
                stuck.push(id);
            }
            if op.unservable.is_some() || verdict != Verdict::Servable {
                verdicts.push((id, verdict));
            }
        }
        for id in stuck {
            self.undo_key_raise(id);
        }
        self.reservations = reservations;
        self.drain.marks = whole.marks;
        self.drain.waits = whole.waits;
        self.head_holds = head_holds;
        for (id, verdict) in verdicts {
            // A size a key floor gave it and no live worker can hold is lowered, not
            // waited on: the next round judges the smaller request.
            if matches!(verdict, Verdict::Unservable(_)) && self.reclamp_key_floor(id) {
                continue;
            }
            self.note(id, verdict, effects);
        }
        for (id, placed) in placed {
            let Placed {
                worker,
                booked,
                device,
                cost,
            } = placed;
            let lease = LeaseId::new(self.term, self.next_seq);
            self.next_seq += 1;
            let op = self.ops.get_mut(&id).expect("queued operations exist");
            self.queue.remove(&(Reverse(op.request.qos.clone()), id));
            op.state = OpState::Leased {
                lease,
                worker: worker.clone(),
                committed: false,
            };
            self.held.insert(
                lease,
                Held {
                    operation: id,
                    booked,
                    cost,
                    device,
                    start_sent: None,
                    granted_at: self.now,
                    started_at: None,
                },
            );
            effects.push(Effect::Commit(ControlRecord::Lease(LeaseGrant {
                lease,
                operation: id,
                worker,
                cost,
            })));
        }
    }

    /// Records this round's verdict on queued operation `id`: tells its waiters when
    /// the reason it waits changes, and proposes its refusal once no worker, cordoned or
    /// not, could run it for the unservable wait. Time spent waiting only for a cordon
    /// does not count: the wait starts again when the work becomes unservable.
    fn note(&mut self, id: OperationId, verdict: Verdict, effects: &mut Vec<Effect>) {
        let now = self.now;
        let wait = self.unservable_wait;
        let op = self.ops.get_mut(&id).expect("queued operations exist");
        let (reason, refusable) = match verdict {
            Verdict::Servable => {
                // Only an operation that was waiting has a servable verdict noted.
                op.unservable = None;
                effects.push(Effect::Waiting(Waiting {
                    operation: id,
                    reason: None,
                }));
                return;
            }
            Verdict::Unservable(reason) => (reason, true),
            Verdict::Cordoned(reason) => (reason, false),
        };
        let unservable = match &mut op.unservable {
            Some(u) if u.reason == reason => u,
            slot => {
                let since = match slot {
                    Some(u) if u.refusable && refusable => u.since,
                    _ => now,
                };
                let text = reason.to_string();
                effects.push(Effect::Waiting(Waiting {
                    operation: id,
                    reason: Some(text.clone()),
                }));
                slot.insert(UnservableWait {
                    since,
                    reason,
                    text,
                    refusable,
                })
            }
        };
        if unservable.refusable && now >= unservable.since.saturating_add(wait) {
            let reason = format!(
                "{} (waited {} s for a worker that can run it)",
                unservable.text,
                wait.as_secs()
            );
            self.queue.remove(&(Reverse(op.request.qos.clone()), id));
            effects.push(Effect::Commit(ControlRecord::Refusal(RefusalRecord {
                operation: id,
                reason,
            })));
        }
    }
}
