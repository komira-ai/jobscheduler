//! The scheduler state machine.

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
mod worker;

pub use recover::{Fate, Recovered, RecoveredLease};
pub use snapshot::{
    Avoided, LeasePhase, LeaseView, MAX_AVOIDED, QueueGroup, QueueView, Snapshot, WaitingGroup,
    platform_label,
};
pub use state::OpState;
pub(crate) use worker::Worker;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use js_types::sched::usage::{CurveSource, LeaseCost};
use js_types::sched::{
    Answer, ControlRecord, Effect, Failure, FarmRun, FarmTime, LeaseGrant, LeaseId, LeaseKind,
    OperationId, Outcome, Qos, Refusal, RefusalRecord, Resources, ResultRecord, StartLease,
    WaiterId, Waiting, WorkKey, WorkerId,
};

use crate::admission::{Admission, Blocked, LoadConfig};
use crate::cordon::Cordons;
use crate::fence::HANDOVER_GRACE;
use crate::input::Request;
use crate::requeue::{Requeue, RequeueReason};
use crate::servable::{Attempt, Placed, Servable, Verdict};
use memory::{MemoryLadder, MemoryState};

/// At most this many leases are granted per [`Event::Tick`] (one log flush per round).
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
    unservable: Option<Unservable>,
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
struct Unservable {
    since: FarmTime,
    reason: String,
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
    /// The worker each blocked head of the queue holds (see [`Scheduler::place`]); the
    /// last round's, kept so the hold does not wander.
    head_holds: BTreeMap<OperationId, WorkerId>,
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
            awaited: BTreeMap::new(),
            other_process: BTreeMap::new(),
            store_down: false,
            grace_until: FarmTime::default(),
            loss_cap: u32::MAX,
            admission: Admission::default(),
            head_holds: BTreeMap::new(),
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
    /// [`Scheduler::new`] places by the booked rule until this is called; the defaults of
    /// [`LoadConfig`] are `On`, 5 s windows, `f` 0.75, `s_cpu` 0.10, `s_mem` 0.
    #[must_use]
    pub fn with_load_config(mut self, config: LoadConfig) -> Self {
        self.admission.configure(config);
        self
    }

    /// This scheduler, holding `curves` for the rule that charges a lease by a curve over
    /// its lifetime. The flat rule does not read them: a key that has a curve is charged
    /// flat and counted ([`LoadCounters::curve_flat`]).
    #[must_use]
    pub fn with_curves(mut self, curves: Arc<dyn CurveSource + Send + Sync>) -> Self {
        self.admission.use_curves(curves);
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
                        reason: Some(u.reason.clone()),
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

    /// Releases `operation`'s holding (booking and lease), if it has one.
    fn release(&mut self, id: OperationId) {
        let op = self.ops.get_mut(&id).expect("released operations exist");
        if let Some((lease, worker)) = op.state.holding() {
            let held = self.held.remove(&lease);
            if let Some((w, held)) = self.workers.get_mut(worker).zip(held) {
                w.booked = w.booked.saturating_sub(held.booked);
                w.load_state.remove(lease);
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
        self.release(id);
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
    /// Under admission by load ([`Scheduler::with_load_config`]) an action has room where
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
        let mut head_holds = BTreeMap::new();
        let mut held_pools = BTreeSet::new();
        for &(_, id) in &self.queue {
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
            let retried = !self.memory.runs(id).is_empty();
            let cost = self.admission.cost_of(id, request, retried);
            let attempt = Attempt {
                request,
                reserved: &reserved,
                avoid: &op.avoid,
                not_whole: key_raised,
                cost,
                lease,
            };
            let mut blocked = None;
            let fitted = placed.len() < round && {
                let fit = servable.fit(&mut self.workers, &mut self.admission, &attempt);
                blocked = fit.blocked;
                fit.placed.map(|p| placed.push((id, p))).is_some()
            };
            let verdict = if fitted {
                Verdict::Servable
            } else {
                servable.verdict(&self.workers, request)
            };
            if !fitted
                && request.kind == LeaseKind::WholeMachine
                && verdict == Verdict::Servable
                && let Some(name) = servable.reserve(
                    &self.workers,
                    request,
                    &reserved,
                    &op.avoid,
                    self.reservations.get(&id),
                )
            {
                reserved.insert(name.clone());
                reservations.insert(id, name);
            }
            // A blocked head holds the worker with the most headroom, one per pool:
            // requests after it in queue order are not placed there, so the worker
            // empties as its leases end instead of being refilled by small work.
            if blocked == Some(Blocked::Load)
                && held_pools.insert(servable.pool(&self.workers, request))
            {
                let attempt = Attempt {
                    request,
                    reserved: &reserved,
                    avoid: &op.avoid,
                    not_whole: key_raised,
                    cost,
                    lease,
                };
                let kept = self.head_holds.get(&id);
                if let Some(name) =
                    servable.hold_for_head(&self.workers, &self.admission, &attempt, kept)
                {
                    self.admission.counters.reservations += u64::from(kept != Some(&name));
                    reserved.insert(name.clone());
                    head_holds.insert(id, name);
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
                effects.push(Effect::Waiting(Waiting {
                    operation: id,
                    reason: Some(reason.clone()),
                }));
                slot.insert(Unservable {
                    since,
                    reason,
                    refusable,
                })
            }
        };
        if unservable.refusable && now >= unservable.since.saturating_add(wait) {
            let reason = format!(
                "{} (waited {} s for a worker that can run it)",
                unservable.reason,
                wait.as_secs()
            );
            self.queue.remove(&(Reverse(op.request.qos.clone()), id));
            effects.push(Effect::Commit(ControlRecord::Refusal(RefusalRecord {
                operation: id,
                reason,
            })));
        }
    }

    /// A committed refusal: the operation is finished, and its waiters are answered.
    /// A refusal of an operation that is no longer queued is stale and dropped.
    fn refusal_committed(&mut self, record: &RefusalRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        if op.state != OpState::Queued {
            return Vec::new();
        }
        op.state = OpState::Refused {
            reason: record.reason.clone(),
        };
        op.unservable = None;
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        self.finished.push_back((self.now, id));
        vec![Effect::Refuse(Refusal {
            operation: id,
            waiters: op.waiters.clone(),
            reason: record.reason.clone(),
        })]
    }

    fn lease_committed(&mut self, grant: &LeaseGrant, effects: &mut Vec<Effect>) {
        let Some(op) = self.ops.get_mut(&grant.operation) else {
            return;
        };
        if op.state.is_done() {
            return;
        }
        op.committed_lease = op.committed_lease.max(Some(grant.lease));
        if let OpState::Leased {
            lease,
            worker,
            committed,
        } = &mut op.state
            && *lease == grant.lease
            && !*committed
        {
            *committed = true;
            let (session, process) = self
                .workers
                .get(&*worker)
                .map_or((0, 0), |w| (w.session, w.process));
            let sent = StartSent {
                at: self.now,
                session,
                process,
            };
            // A leased operation's lease is always held: no branch on it.
            let (booked, device) =
                self.held
                    .get_mut(&*lease)
                    .map_or((op.request.resources, None), |held| {
                        held.start_sent = Some(sent);
                        (held.booked, held.device.clone())
                    });
            effects.push(Effect::Start(StartLease {
                worker: worker.clone(),
                lease: *lease,
                operation: grant.operation,
                key: op.request.key.clone(),
                kind: op.request.kind,
                resources: booked,
                fence: op.request.fence(),
                device,
            }));
        }
    }

    fn started(&mut self, id: OperationId, lease: LeaseId) {
        let Some(op) = self.ops.get_mut(&id) else {
            return;
        };
        if let OpState::Leased {
            lease: held,
            worker,
            committed: true,
        } = &op.state
            && *held == lease
        {
            op.state = OpState::Running {
                lease,
                worker: worker.clone(),
            };
            // A leased operation's lease is always held: no branch on it.
            let now = self.now;
            let () = self
                .held
                .get_mut(&lease)
                .map_or((), |held| held.started_at = Some(now));
        }
    }

    fn report(&mut self, id: OperationId, lease: LeaseId, outcome: Outcome) -> Vec<Effect> {
        let Some(op) = self.ops.get_mut(&id) else {
            return Vec::new();
        };
        let current = match &op.state {
            OpState::Leased {
                lease, committed, ..
            } => committed.then_some(*lease),
            OpState::Running { lease, .. } => Some(*lease),
            _ => None,
        };
        if current != Some(lease) || op.result_proposed {
            return Vec::new();
        }
        op.result_proposed = true;
        vec![Effect::Commit(ControlRecord::Result(ResultRecord {
            lease,
            operation: id,
            outcome,
        }))]
    }

    fn result_committed(&mut self, record: &ResultRecord) -> Vec<Effect> {
        let id = record.operation;
        let Some(op) = self.ops.get(&id) else {
            return Vec::new();
        };
        if op.state.is_done() || op.committed_lease != Some(record.lease) {
            return Vec::new();
        }
        if let Outcome::Failed(kill @ (Failure::OutOfMemory | Failure::NodeMemoryPressure)) =
            record.outcome
            && self.rerun_after_memory_kill(record, kill)
        {
            return Vec::new();
        }
        if let Outcome::Failed(Failure::Lost(why)) = record.outcome
            && self.rerun_after_lost(record, why)
        {
            return Vec::new();
        }
        // The scheduler's own give-up for a lease that never started is not a run to charge.
        if record.outcome == Outcome::Failed(Failure::Infra)
            && !self.ops[&id].gave_up
            && self.rerun_after_infra(record)
        {
            return Vec::new();
        }
        self.release(id);
        let op = self.ops.get_mut(&id).expect("checked above");
        op.state = match record.outcome {
            Outcome::Completed { action_result } => OpState::Completed {
                lease: record.lease,
                action_result,
            },
            Outcome::Failed(failure) => OpState::Failed {
                lease: record.lease,
                failure,
            },
        };
        // A queued operation (expired, not yet re-granted) leaves the queue too.
        self.queue.remove(&(Reverse(op.request.qos.clone()), id));
        if self.in_flight.get(&op.request.key) == Some(&id) {
            self.in_flight.remove(&op.request.key);
        }
        self.finished.push_back((self.now, id));
        vec![Effect::Answer(Answer {
            operation: id,
            lease: record.lease,
            waiters: op.waiters.clone(),
            outcome: record.outcome,
            memory_runs: self.memory.runs(id).to_vec(),
            farm_runs: op.farm_runs.clone(),
        })]
    }
}
