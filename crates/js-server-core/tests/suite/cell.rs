//! The core in a simulated cell: a leader running [`Core`] with no store, and two workers,
//! on a network that delays, duplicates and reorders messages. In the base run one worker
//! is cut off long enough for its leases to expire and be given to the other.
//!
//! The leader is the server's side of the worker protocol over the core's host API: only
//! the first `Hello` of a session registers, a `Hello` resent on it is a resize, a
//! heartbeat of a replaced session is dropped, every `Start` goes to the session its
//! worker registered, and a `Result` is taken only from the node that holds its lease. The
//! control log is the core's own: a record is committed as it is proposed. Workers
//! heartbeat every 5 s, keep a [`SelfFence`], run every action for 100 s, and resend
//! unacknowledged reports with each heartbeat.
//!
//! The checks, over a seed sweep: every operation is answered exactly once; no worker
//! has anything booked once all are; no self-fenced operation runs on two workers at once;
//! and a seed replays to the trace the checked-in hash records. These runs are the
//! scheduler's own cell (`js-sched`, `tests/sim_cell.rs`) with the core as the leader; the
//! scheduler's golden hashes are not touched.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::common::{begin, block_on};
use js_sched::{DaemonInstance, Request, SelfFence};
use js_server_core::{Adapter, Beat, Config, Core, Delivery, Reported, Stage, Standing, View};
use js_sim::golden::GoldenFile;
use js_sim::{
    Chance, Event, Faults, Node, NodeId, NodeInput, Output, Partition, Sim, TraceHash, text,
};
use js_types::sched::{
    Answer, Effect, FarmTime, FencePolicy, LeaseGrant, LeaseId, LeaseKind, NodePhase, OperationId,
    Outcome, Qos, Refusal, Resources, ResultRef, StartLease, StateMachine, WorkKey, WorkerId,
};

const SEEDS: u64 = 24;
const GIB: u64 = 1 << 30;
const TICK: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(5);
const RUN_FOR: Duration = Duration::from_secs(100);
/// When the callers submit, and the leader's timer tag for it.
const SUBMIT_AFTER: Duration = Duration::from_secs(2);
const SUBMIT: u64 = 1;
const OPS: u64 = 12;
/// Worker-1 is cut off from everyone for this window, longer than the grace G.
const CUT: (u64, u64) = (5_000, 80_000);
const END: u64 = 400_000;
/// When every worker's node report changes, and it resends its `Hello`.
const REPORT_CHANGE_AT: u64 = 45_000;
/// The timer tag of a worker's node report change (run timers count up from 1).
const REPORT_CHANGE: u64 = u64::MAX;

#[derive(Clone, Debug)]
enum Msg {
    /// The wire's `Hello`; `session` stands for the stream it is sent on.
    Hello {
        capacity: Resources,
        session: u64,
    },
    Heartbeat {
        sent_at: FarmTime,
        session: u64,
        running: Vec<LeaseId>,
    },
    Ack {
        sent_at: FarmTime,
        session: u64,
    },
    Start {
        start: StartLease,
        session: u64,
    },
    Report {
        operation: OperationId,
        lease: LeaseId,
        outcome: Outcome,
    },
    ReportAck {
        lease: LeaseId,
    },
}

fn trace_msg(msg: &Msg) -> String {
    match msg {
        Msg::Hello { capacity, session } => {
            format!("hello(session={session} {})", text::resources(capacity))
        }
        Msg::Heartbeat {
            sent_at,
            session,
            running,
        } => format!(
            "heartbeat(sent_at={} session={session} running={})",
            text::time(*sent_at),
            text::list(running, |l| text::lease(*l))
        ),
        Msg::Ack { sent_at, session } => {
            format!("ack(sent_at={} session={session})", text::time(*sent_at))
        }
        Msg::Start { start, session } => format!("start(session={session} {})", text::start(start)),
        Msg::Report {
            operation,
            lease,
            outcome,
        } => format!(
            "report(op={} lease={} outcome={})",
            text::operation(*operation),
            text::lease(*lease),
            text::outcome(*outcome)
        ),
        Msg::ReportAck { lease } => format!("report_ack(lease={})", text::lease(*lease)),
    }
}

/// The core's adapter: a `Start` is a message to the session its worker registered, an
/// answer is kept for the checks, and nothing else matters to a cell of workers.
#[derive(Default)]
struct Wire {
    sessions: BTreeMap<WorkerId, u64>,
    outbox: Vec<Output<Msg>>,
    answers: Vec<Answer>,
}

impl Adapter for Wire {
    fn stream_open(&self, _: &WorkerId, _: js_server_core::Stream) -> bool {
        true
    }

    fn name(&self, _: &View<'_>, _: OperationId) -> String {
        String::new()
    }

    fn phased(&mut self, _: &WorkerId, _: &NodePhase, _: &NodePhase) {}

    fn offer(&mut self, _: &View<'_>, _: &LeaseGrant) {}

    fn start(&mut self, _: &View<'_>, start: &StartLease) {
        let session = self.sessions[&start.worker];
        self.outbox.push(Output::Send {
            to: NodeId::new(start.worker.as_str()),
            msg: Msg::Start {
                start: start.clone(),
                session,
            },
        });
    }

    fn cancel(&mut self, _: &View<'_>, _: &WorkerId, _: LeaseId) {}

    fn show(&mut self, _: &View<'_>, _: OperationId, _: &Stage) {}

    fn refuse(&mut self, _: &View<'_>, refusal: &Refusal) {
        panic!("an operation was refused: {refusal:?}");
    }

    fn requeued(&mut self, _: &js_sched::Requeue) {}

    fn settle(&mut self, _: &View<'_>, answer: &Answer, _: Option<&WorkerId>) -> Delivery {
        self.answers.push(answer.clone());
        Box::pin(std::future::ready(()))
    }

    fn forget_dropped(&mut self, _: &dyn Fn(OperationId) -> bool) {}

    fn forget_parcel(&mut self, _: LeaseId) {}

    fn retain_parcels(&mut self, _: &dyn Fn(LeaseId) -> bool) {}
}

fn leader_id() -> NodeId {
    NodeId::from("leader")
}

fn work(n: u64) -> Request {
    Request {
        key: WorkKey::new("main", format!("action-{n}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        needs: js_caps::Request::default(),
        // Every fourth action is networked, so it self-fences.
        hermetic: !n.is_multiple_of(4),
        do_not_cache: false,
    }
}

fn report_of(capacity: Resources) -> Reported {
    let caps = js_caps::NodeCaps::from_report([("arch", "arm64")])
        .expect("a node report")
        .with_drivers(["container"]);
    Reported::serving(capacity, caps)
}

struct Leader {
    core: Core<Wire>,
    /// The newest session each worker registered, and the stream the core numbered for it.
    sessions: BTreeMap<NodeId, (u64, js_server_core::Stream)>,
    out: Vec<Output<Msg>>,
}

impl Leader {
    fn new() -> Self {
        begin();
        Self {
            core: Core::new(&Config::new(1), Wire::default()),
            sessions: BTreeMap::new(),
            out: Vec::new(),
        }
    }

    /// Moves what the adapter sent into the leader's outputs, after what the leader sent
    /// itself while handling the input.
    fn flush(&mut self) {
        let sent = self.core.with(|_, wire| std::mem::take(&mut wire.outbox));
        self.out.extend(sent);
    }

    fn send(&mut self, to: NodeId, msg: Msg) {
        self.out.push(Output::Send { to, msg });
    }

    fn apply(&mut self, now: FarmTime, event: Event<Msg>) {
        match event {
            Event::Start => {
                // The core places work after every input, so a worker that registers first
                // takes what it has room for: the work comes once both have registered.
                self.out.push(Output::Timer {
                    after: SUBMIT_AFTER,
                    tag: SUBMIT,
                });
                self.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
            }
            Event::Timer { tag: SUBMIT } => {
                for n in 0..OPS {
                    self.core
                        .submit(now, work(n), |_, _| {})
                        .expect("not fenced");
                }
            }
            Event::Timer { .. } => {
                self.out.push(Output::Timer {
                    after: TICK,
                    tag: 0,
                });
                block_on(self.core.tick(now));
            }
            Event::Message { from, msg } => self.receive(now, from, msg),
        }
        self.flush();
    }

    fn receive(&mut self, now: FarmTime, from: NodeId, msg: Msg) {
        let worker = WorkerId::new(from.as_str());
        let current = self.sessions.get(&from).copied();
        match msg {
            // Only the first `Hello` of a session registers; one resent on it is a resize.
            Msg::Hello { capacity, session } => match current {
                Some((newest, stream)) if session == newest => {
                    self.core
                        .resize(now, &worker, stream, report_of(capacity), |_| {})
                        .expect("heard");
                }
                Some((newest, _)) if session < newest => {}
                _ => {
                    // Each session is another daemon process.
                    let instance = DaemonInstance::new(format!("{}#{session}", from.as_str()));
                    let predecessor = DaemonInstance::new("");
                    let name = worker.clone();
                    let stream = self
                        .core
                        .register(
                            now,
                            &worker,
                            instance,
                            predecessor,
                            report_of(capacity),
                            move |wire, _| {
                                wire.sessions.insert(name, session);
                            },
                        )
                        .expect("heard");
                    self.sessions.insert(from, (session, stream));
                }
            },
            Msg::Heartbeat {
                sent_at,
                session,
                running,
            } => {
                // A heartbeat of an older session belongs to a closed stream, and one of a
                // session whose `Hello` has not arrived is heard once it has.
                let Some((_, stream)) = current.filter(|(newest, _)| session == *newest) else {
                    return;
                };
                self.send(from, Msg::Ack { sent_at, session });
                let beat = Beat::new(sent_at.as_millis(), running);
                block_on(self.core.heartbeat(now, &worker, stream, beat)).expect("heard");
            }
            Msg::Report {
                operation,
                lease,
                outcome,
            } => {
                self.send(from, Msg::ReportAck { lease });
                let held = self.core.standing(&worker, lease).expect("heard");
                if held == Standing::Current(operation) {
                    let report = js_server_core::Report {
                        operation,
                        lease,
                        outcome,
                    };
                    block_on(self.core.report(now, &worker, report, |_| {}));
                }
            }
            other => panic!("leader got {other:?}"),
        }
    }
}

/// How a worker misbehaves, while it keeps heartbeating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// Every copy of the first `Start` sent to this worker is lost.
    DropFirstStart,
}

struct Run {
    operation: OperationId,
    fence: FencePolicy,
    started: FarmTime,
    ended: Option<FarmTime>,
}

struct Worker {
    capacity: Resources,
    fault: Fault,
    session: u64,
    /// The lease whose `Start` was lost ([`Fault::DropFirstStart`]).
    dropped: Option<LeaseId>,
    fence: SelfFence,
    runs: BTreeMap<LeaseId, Run>,
    unacked: BTreeMap<LeaseId, (OperationId, Outcome)>,
    timers: BTreeMap<u64, LeaseId>,
    out: Vec<Output<Msg>>,
}

impl Worker {
    fn new(capacity: Resources, fault: Fault) -> Self {
        Self {
            capacity,
            fault,
            session: 1,
            dropped: None,
            fence: SelfFence::new(),
            runs: BTreeMap::new(),
            unacked: BTreeMap::new(),
            timers: BTreeMap::new(),
            out: Vec::new(),
        }
    }

    fn send(&mut self, msg: Msg) {
        self.out.push(Output::Send {
            to: leader_id(),
            msg,
        });
    }

    /// Stops self-fenced runs at their fence deadline.
    fn enforce_fence(&mut self, now: FarmTime) {
        let deadline = self.fence.deadline().unwrap_or_default();
        for run in self.runs.values_mut() {
            if run.fence == FencePolicy::SelfFence && run.ended.is_none() && now >= deadline {
                run.ended = Some(deadline.max(run.started));
            }
        }
    }

    fn start(&mut self, now: FarmTime, start: &StartLease, session: u64) {
        // Sent to an earlier session: it went down with that stream.
        if session != self.session {
            return;
        }
        if self.fault == Fault::DropFirstStart
            && self.dropped.is_none_or(|lease| lease == start.lease)
        {
            self.dropped = Some(start.lease);
            return;
        }
        if self.runs.contains_key(&start.lease) {
            return;
        }
        if start.fence == FencePolicy::SelfFence && !self.fence.allows(now) {
            self.unacked.insert(
                start.lease,
                (
                    start.operation,
                    Outcome::Failed(js_types::sched::Failure::Infra),
                ),
            );
            return;
        }
        let run = Run {
            operation: start.operation,
            fence: start.fence,
            started: now,
            ended: None,
        };
        self.runs.insert(start.lease, run);
        let tag = 1 + self.timers.len() as u64;
        self.timers.insert(tag, start.lease);
        self.out.push(Output::Timer {
            after: RUN_FOR,
            tag,
        });
    }

    fn finish(&mut self, now: FarmTime, lease: LeaseId) {
        let run = self.runs.get_mut(&lease).expect("timers name runs");
        if run.ended.is_some() {
            return;
        }
        run.ended = Some(now);
        let operation = run.operation;
        let outcome = Outcome::Completed {
            action_result: ResultRef::new([7; 32], 1_000 + operation.0),
        };
        self.unacked.insert(lease, (operation, outcome));
        self.send(Msg::Report {
            operation,
            lease,
            outcome,
        });
    }

    /// The leases this worker reports holding: runs not ended, and ended runs whose
    /// result is not yet acknowledged.
    fn running(&self) -> Vec<LeaseId> {
        let mut held: BTreeSet<LeaseId> = self.unacked.keys().copied().collect();
        held.extend(
            self.runs
                .iter()
                .filter(|(_, run)| run.ended.is_none())
                .map(|(lease, _)| *lease),
        );
        held.into_iter().collect()
    }

    fn hello(&mut self) {
        let (capacity, session) = (self.capacity, self.session);
        self.send(Msg::Hello { capacity, session });
    }

    /// Sends one heartbeat and resends every unacknowledged report.
    fn beat(&mut self, now: FarmTime) {
        let running = self.running();
        let session = self.session;
        self.send(Msg::Heartbeat {
            sent_at: now,
            session,
            running,
        });
        let resend: Vec<_> = self.unacked.iter().map(|(l, v)| (*l, *v)).collect();
        for (lease, (operation, outcome)) in resend {
            self.send(Msg::Report {
                operation,
                lease,
                outcome,
            });
        }
        self.out.push(Output::Timer {
            after: HEARTBEAT,
            tag: 0,
        });
    }

    fn apply(&mut self, now: FarmTime, event: Event<Msg>) {
        self.enforce_fence(now);
        match event {
            Event::Start => {
                let after = Duration::from_millis(REPORT_CHANGE_AT);
                self.out.push(Output::Timer {
                    after,
                    tag: REPORT_CHANGE,
                });
                self.hello();
                self.beat(now);
            }
            Event::Timer { tag: 0 } => self.beat(now),
            Event::Timer { tag: REPORT_CHANGE } => self.hello(),
            Event::Timer { tag } => {
                let lease = self.timers[&tag];
                self.finish(now, lease);
            }
            Event::Message { msg, .. } => match msg {
                Msg::Ack { sent_at, session } => {
                    if session == self.session {
                        self.fence.acknowledged(sent_at);
                    }
                }
                Msg::Start { start, session } => self.start(now, &start, session),
                Msg::ReportAck { lease } => {
                    self.unacked.remove(&lease);
                }
                other => panic!("worker got {other:?}"),
            },
        }
    }
}

// One cell per simulated node, built once per run: the leader's size is no cost.
#[allow(clippy::large_enum_variant)]
enum Cell {
    Leader(Leader),
    Worker(Worker),
}

impl StateMachine for Cell {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            Cell::Leader(leader) => leader.apply(input.now, input.event),
            Cell::Worker(worker) => worker.apply(input.now, input.event),
        }
        Vec::new()
    }
}

impl Node for Cell {
    type Msg = Msg;

    fn trace_msg(msg: &Msg) -> String {
        trace_msg(msg)
    }

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Cell::Leader(leader) => std::mem::take(&mut leader.out),
            Cell::Worker(worker) => std::mem::take(&mut worker.out),
        }
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    cut: bool,
    worker_1: Fault,
}

const BASE: Scenario = Scenario {
    cut: true,
    worker_1: Fault::None,
};
const DROP_FIRST_START: Scenario = Scenario {
    cut: false,
    worker_1: Fault::DropFirstStart,
};

fn run(seed: u64, scenario: Scenario) -> Sim<Cell> {
    let faults = Faults {
        min_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(50),
        duplicate: Chance::percent(5),
        reorder: Chance::percent(5),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    sim.add_node("leader", Cell::Leader(Leader::new()));
    // Placement spreads the twelve operations over the cores: four land on worker-1
    // (6 cores) and are cut off, eight on worker-2 (12 cores).
    let worker_1 = Worker::new(Resources::new(6_000, 64 * GIB), scenario.worker_1);
    sim.add_node("worker-1", Cell::Worker(worker_1));
    let worker_2 = Worker::new(Resources::new(12_000, 64 * GIB), Fault::None);
    sim.add_node("worker-2", Cell::Worker(worker_2));
    if scenario.cut {
        let group = vec![NodeId::from("worker-1")];
        sim.partition_at(FarmTime::from_millis(CUT.0), Partition::new([group]));
        sim.partition_at(FarmTime::from_millis(CUT.1), Partition::none());
    }
    sim.run_until(FarmTime::from_millis(END));
    sim
}

fn leader(sim: &Sim<Cell>) -> &Leader {
    match sim.node(&leader_id()) {
        Some(Cell::Leader(leader)) => leader,
        _ => unreachable!("the leader is a leader"),
    }
}

fn workers(sim: &Sim<Cell>) -> impl Iterator<Item = (&NodeId, &Worker)> {
    sim.nodes().filter_map(|(id, node)| match node {
        Cell::Worker(worker) => Some((id, worker)),
        Cell::Leader(_) => None,
    })
}

/// The answers the core delivered, in order.
fn answers(sim: &Sim<Cell>) -> Vec<Answer> {
    leader(sim).core.with(|_, wire| wire.answers.clone())
}

/// Checks that every operation was answered exactly once, and returns the answering leases.
fn assert_answered_once(sim: &Sim<Cell>, seed: u64) -> BTreeMap<OperationId, LeaseId> {
    let mut seen = BTreeMap::new();
    for answer in answers(sim) {
        assert!(
            seen.insert(answer.operation, answer.lease).is_none(),
            "seed {seed}: {} answered twice",
            answer.operation
        );
    }
    assert_eq!(seen.len() as u64, OPS, "seed {seed}: unanswered operations");
    seen
}

/// Checks that once every operation is answered, no worker has anything booked.
fn assert_bookings_released(sim: &Sim<Cell>, seed: u64) {
    for (id, _) in workers(sim) {
        let who = WorkerId::new(id.as_str());
        let booked = leader(sim).core.with(|view, _| view.sched().booked(&who));
        assert_eq!(
            booked,
            Some(Resources::default()),
            "seed {seed}: {id} still booked at the end"
        );
    }
}

/// Checks that no self-fenced operation ran on two workers at once, and returns how many
/// ran more than once.
fn assert_never_twice_at_once(sim: &Sim<Cell>, seed: u64) -> usize {
    let mut spans: BTreeMap<OperationId, Vec<(u64, u64)>> = BTreeMap::new();
    for (_, worker) in workers(sim) {
        for run in worker.runs.values() {
            if run.fence == FencePolicy::SelfFence {
                let end = run.ended.map_or(END, FarmTime::as_millis);
                spans
                    .entry(run.operation)
                    .or_default()
                    .push((run.started.as_millis(), end));
            }
        }
    }
    let rerun = spans.values().filter(|s| s.len() > 1).count();
    for (op, mut s) in spans {
        s.sort_unstable();
        for pair in s.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "seed {seed}: {op} ran twice at once: {pair:?}"
            );
        }
    }
    rerun
}

/// Catches: a result accepted from a lease that was superseded (a late result after
/// re-dispatch), a duplicate result answered twice, an operation never answered, a
/// booking that leaks, or a self-fenced run that outlives the grace on a cut-off worker.
/// The cut-off worker's runs finish after their operations were given to the other worker
/// and report late; those reports must lose to the new lease.
#[test]
fn a_cut_off_worker_loses_its_leases_and_each_operation_is_answered_once() {
    let mut networked_reruns = 0;
    for seed in 0..SEEDS {
        let sim = run(seed, BASE);
        let answered = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        networked_reruns += assert_never_twice_at_once(&sim, seed);
        // The scenario did what it claims: some answer comes from a re-dispatch.
        assert!(answered.values().any(|l| l.seq >= OPS), "seed {seed}");
    }
    assert!(networked_reruns > 0, "no networked operation ran again");
}

/// Catches: a committed lease whose `Start` never reached a worker that stays connected
/// kept for good (its operation never answered, its booking leaked), and one given up
/// before the start grace.
#[test]
fn a_start_lost_on_a_live_session_is_granted_again() {
    for seed in 0..SEEDS {
        let sim = run(seed, DROP_FIRST_START);
        let answered = assert_answered_once(&sim, seed);
        assert_bookings_released(&sim, seed);
        assert_never_twice_at_once(&sim, seed);
        let lost = workers(&sim)
            .find_map(|(_, worker)| worker.dropped)
            .expect("a Start was lost");
        assert!(
            answered.values().all(|lease| *lease != lost),
            "seed {seed}: answered by the lease whose Start was lost"
        );
    }
}

/// Catches: anything in the core or this cell that depends on more than the seed (a hashed
/// collection, a clock): two runs of one seed must trace identically.
#[test]
fn a_seed_replays_exactly() {
    let hash = |seed| -> TraceHash { run(seed, BASE).trace_hash() };
    assert_eq!(hash(7), hash(7));
    assert_ne!(hash(7), hash(8));
}

fn golden() -> GoldenFile {
    GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        "core-cell",
        "cargo test -p js-server-core --test suite -- --ignored regenerate_golden_hashes",
    )
}

/// The trace hash of the cell under each scenario and three seeds.
fn golden_runs() -> Vec<(String, TraceHash)> {
    let scenarios = [("base", BASE), ("drop-first-start", DROP_FIRST_START)];
    let mut runs = Vec::new();
    for (name, scenario) in scenarios {
        for seed in 0..3 {
            let hash = run(seed, scenario).trace_hash();
            runs.push((format!("{name}/seed-{seed}"), hash));
        }
    }
    runs
}

/// Catches: a change to the core, the cell or the trace text that alters what a scenario
/// decides, or how its trace reads, without the author meaning it.
#[test]
fn traces_match_their_golden_hashes() {
    golden().assert_matches(&golden_runs());
}

/// Rewrites the cell lines of `tests/golden/trace-hashes.txt`.
#[test]
#[ignore = "regenerates the golden trace hashes; run it only on purpose"]
fn regenerate_golden_hashes() {
    golden().regenerate(&golden_runs()).unwrap();
}
