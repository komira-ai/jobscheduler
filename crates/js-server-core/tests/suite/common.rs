//! The adapter and the helpers the core's tests share: an adapter that records every hook
//! it is called through, a writer a test can make refuse, and short ways to bring a worker
//! up and submit work.

#![allow(dead_code, reason = "each test binary uses some of the rig")]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, NodePhase, OpState, Request, Requeue};
use js_server_core::{
    Adapter, Admitted, Attempt, Beat, Config, Core, Delivery, Denied, Lowered, NotQueued, Report,
    Reported, Settles, Stage, Stream, Taken, View, Writer,
};
use js_store::Ending;
use js_types::sched::{
    Answer, Effect, FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Refusal, Resources,
    StartLease, WaiterId, WorkKey, WorkerId,
};

pub const GIB: u64 = 1 << 30;

/// Lines the rig, the writer and the deliveries append to, in the order they happen.
pub type Trace = Arc<Mutex<Vec<String>>>;

/// One hook the core called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Phased {
        worker: WorkerId,
        before: NodePhase,
        after: NodePhase,
    },
    Offer(LeaseId),
    Start {
        lease: LeaseId,
        worker: WorkerId,
        stream: Option<Stream>,
        beat: u64,
    },
    Cancel(WorkerId, LeaseId),
    Show(OperationId, Stage),
    Refuse(OperationId),
    Requeued(LeaseId),
    Settle {
        operation: OperationId,
        lease: LeaseId,
        ack: Option<WorkerId>,
    },
    Forgot(OperationId),
    ForgotParcel(LeaseId),
    Adopted(OperationId, WaiterId, Option<Vec<u8>>),
    /// The callers shown a stage of an operation another frontend runs.
    ShowCallers(Vec<WaiterId>, Stage),
    /// The callers of a finished operation another frontend ran, kept.
    KeptFinished(OperationId, Vec<WaiterId>),
    /// An operation another frontend ran ended, with its `done/` value.
    Followed(OperationId, Vec<u8>),
    ShowWatched(OperationId, Stage),
}

/// One of the clock and report hooks the core called, or the settle that followed one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hook {
    Clock(FarmTime),
    /// `before_report`: what the view showed of the report's operation.
    Before {
        now: FarmTime,
        worker: WorkerId,
        report: Report,
        open: bool,
        waiters: Vec<WaiterId>,
    },
    /// `after_report`.
    After {
        now: FarmTime,
        worker: WorkerId,
        report: Report,
        answers: Vec<(OperationId, Vec<WaiterId>)>,
        taken: Taken,
        open: bool,
    },
    /// `settle`, so the order against the report hooks shows.
    Settle(LeaseId),
    /// `stream_open`, which the core asks while it claims a node before a feed.
    StreamOpen(WorkerId),
}

/// What `adopted` was told of an operation besides its caller and parcel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adoption {
    pub operation: OperationId,
    pub request: Request,
    pub lease: Option<LeaseGrant>,
    pub size_key: Option<u64>,
}

/// An adapter that keeps what its hooks were told.
#[derive(Default)]
pub struct Rig {
    /// The clock and report hooks, and settles, in the order they were called.
    pub hooks: Mutex<Vec<Hook>>,
    pub trace: Trace,
    pub calls: Vec<Call>,
    /// What each `adopted` was told of its operation, in order.
    pub adoptions: Vec<Adoption>,
    /// The streams whose connection is open.
    pub open: BTreeSet<(WorkerId, Stream)>,
    /// The leases a report was parked for.
    pub parked: BTreeSet<LeaseId>,
    /// The callers each submit enrolled.
    pub callers: BTreeMap<WaiterId, Admitted>,
    /// The operations answered or refused whose callers the rig still keeps.
    pub kept: BTreeSet<OperationId>,
    /// The operations whose delivery ran.
    pub delivered: Arc<Mutex<Vec<OperationId>>>,
    /// The resent reports that reached the rig.
    pub resized: u32,
    /// The rig cannot say how an operation ended.
    pub no_ending: bool,
    /// The names the core asked for, in order.
    pub named: Mutex<Vec<String>>,
    /// The callers that gave up: no longer listening.
    pub gone: BTreeSet<WaiterId>,
    /// What each caller of an operation another frontend runs was last shown.
    pub shown: BTreeMap<WaiterId, Stage>,
    /// The operations clients ask for by name, and what each was last shown.
    pub watching: BTreeMap<OperationId, Option<Stage>>,
    /// The summary name each size key's floor decays with ([`Adapter::summary_name`]).
    pub names: BTreeMap<u64, String>,
    /// The names each `key_names` call was told, in order.
    pub key_names_told: Vec<Vec<(u64, String)>>,
    /// The leases `learned_requeue` was told were given up, in order.
    pub learned: Vec<LeaseId>,
    /// The floors the next poll of `lowered` hands to the core.
    pub to_lower: Vec<Lowered>,
}

impl Rig {
    fn note(&self, line: String) {
        self.trace.lock().expect("trace").push(line);
    }

    fn hook(&self, hook: Hook) {
        self.hooks.lock().expect("hooks").push(hook);
    }
}

impl Adapter for Rig {
    fn clock(&mut self, now: FarmTime) {
        self.hook(Hook::Clock(now));
    }

    fn before_report(
        &mut self,
        view: &View<'_>,
        now: FarmTime,
        worker: &WorkerId,
        report: &Report,
    ) {
        let sched = view.sched();
        self.hook(Hook::Before {
            now,
            worker: worker.clone(),
            report: *report,
            open: sched.state(report.operation).is_some_and(open),
            waiters: sched
                .waiters(report.operation)
                .map_or_else(Vec::new, <[WaiterId]>::to_vec),
        });
    }

    fn after_report(
        &mut self,
        view: &View<'_>,
        now: FarmTime,
        worker: &WorkerId,
        report: &Report,
        answers: &[Answer],
        taken: Taken,
    ) {
        self.hook(Hook::After {
            now,
            worker: worker.clone(),
            report: *report,
            answers: answers
                .iter()
                .map(|answer| (answer.operation, answer.waiters.clone()))
                .collect(),
            taken,
            open: view.sched().state(report.operation).is_some_and(open),
        });
    }

    fn stream_open(&self, worker: &WorkerId, stream: Stream) -> bool {
        self.hook(Hook::StreamOpen(worker.clone()));
        self.open.contains(&(worker.clone(), stream))
    }

    /// Names the operations the scheduler still holds, as an adapter that looks the name up
    /// from the operation's callers does; one it dropped has none.
    fn name(&self, view: &View<'_>, operation: OperationId) -> String {
        let name = if view.sched().state(operation).is_some() {
            format!("operations/{}", operation.0)
        } else {
            String::new()
        };
        self.named.lock().expect("named").push(name.clone());
        name
    }

    fn phased(&mut self, worker: &WorkerId, before: &NodePhase, after: &NodePhase) {
        self.calls.push(Call::Phased {
            worker: worker.clone(),
            before: before.clone(),
            after: after.clone(),
        });
    }

    fn offer(&mut self, _: &View<'_>, grant: &LeaseGrant) {
        self.note(format!("offer {}", grant.lease));
        self.calls.push(Call::Offer(grant.lease));
    }

    fn start(&mut self, view: &View<'_>, start: &StartLease) {
        self.note(format!("start {}", start.lease));
        let link = view.link(&start.worker);
        self.calls.push(Call::Start {
            lease: start.lease,
            worker: start.worker.clone(),
            stream: link.map(|link| link.stream),
            beat: link.map_or(0, |link| link.newest_beat),
        });
    }

    fn cancel(&mut self, _: &View<'_>, worker: &WorkerId, lease: LeaseId) {
        self.calls.push(Call::Cancel(worker.clone(), lease));
    }

    fn show(&mut self, _: &View<'_>, operation: OperationId, stage: &Stage) {
        self.calls.push(Call::Show(operation, stage.clone()));
    }

    fn refuse(&mut self, _: &View<'_>, refusal: &Refusal) {
        self.kept.insert(refusal.operation);
        self.calls.push(Call::Refuse(refusal.operation));
    }

    fn requeued(&mut self, requeue: &Requeue) {
        self.calls.push(Call::Requeued(requeue.lease));
    }

    fn summary_name(&self, size_key: u64) -> Option<String> {
        self.names.get(&size_key).cloned()
    }

    fn key_names(&mut self, names: &[(u64, String)]) {
        self.key_names_told.push(names.to_vec());
    }

    fn learned_requeue(&mut self, _: &View<'_>, requeue: &Requeue) {
        self.learned.push(requeue.lease);
    }

    fn lowered(&mut self) -> Vec<Lowered> {
        std::mem::take(&mut self.to_lower)
    }

    fn settle(&mut self, _: &View<'_>, answer: &Answer, ack: Option<&WorkerId>) -> Delivery {
        self.note(format!("settle {}", answer.lease));
        self.hook(Hook::Settle(answer.lease));
        self.parked.remove(&answer.lease);
        self.kept.insert(answer.operation);
        self.calls.push(Call::Settle {
            operation: answer.operation,
            lease: answer.lease,
            ack: ack.cloned(),
        });
        let (trace, delivered, operation) = (
            Arc::clone(&self.trace),
            Arc::clone(&self.delivered),
            answer.operation,
        );
        Box::pin(async move {
            trace
                .lock()
                .expect("trace")
                .push(format!("deliver {}", operation.0));
            delivered.lock().expect("delivered").push(operation);
        })
    }

    fn forget_dropped(&mut self, held: &dyn Fn(OperationId) -> bool) {
        let gone: Vec<OperationId> = self.kept.iter().copied().filter(|op| !held(*op)).collect();
        for operation in gone {
            self.kept.remove(&operation);
            self.calls.push(Call::Forgot(operation));
        }
    }

    fn forget_parcel(&mut self, lease: LeaseId) {
        self.parked.remove(&lease);
        self.calls.push(Call::ForgotParcel(lease));
    }

    fn retain_parcels(&mut self, waits: &dyn Fn(LeaseId) -> bool) {
        self.parked.retain(|lease| waits(*lease));
    }

    fn ending(&self, effect: &Effect, retention: Duration) -> Option<Ending> {
        let done = match effect {
            Effect::Answer(answer) => format!("done {}", answer.lease),
            Effect::Refuse(refusal) => format!("refused {}", refusal.operation),
            _ => return None,
        };
        let ending = Ending::new(done.into_bytes(), retention);
        (!self.no_ending).then_some(ending)
    }

    fn adopted(
        &mut self,
        operation: OperationId,
        waiter: WaiterId,
        parcel: Option<&[u8]>,
        request: &Request,
        lease: Option<&LeaseGrant>,
        size_key: Option<u64>,
    ) {
        self.adoptions.push(Adoption {
            operation,
            request: request.clone(),
            lease: lease.cloned(),
            size_key,
        });
        let parcel = parcel.map(<[u8]>::to_vec);
        self.calls.push(Call::Adopted(operation, waiter, parcel));
    }

    fn waiting_among(&self, callers: &[WaiterId]) -> Vec<WaiterId> {
        let waiting = |caller: &WaiterId| {
            let done = matches!(self.shown.get(caller), Some(Stage::Done(_)));
            !self.gone.contains(caller) && !done
        };
        callers.iter().copied().filter(waiting).collect()
    }

    fn show_callers(&mut self, callers: &[WaiterId], stage: &Stage) {
        for caller in callers {
            self.shown.insert(*caller, stage.clone());
        }
        self.calls
            .push(Call::ShowCallers(callers.to_vec(), stage.clone()));
    }

    fn keep_finished(&mut self, operation: OperationId, callers: Vec<WaiterId>) {
        self.kept.insert(operation);
        self.calls.push(Call::KeptFinished(operation, callers));
    }

    fn followed(&mut self, operation: OperationId, ended: &[u8]) {
        self.calls.push(Call::Followed(operation, ended.to_vec()));
    }

    fn watched(&mut self) -> Vec<OperationId> {
        let open = |stage: &Option<Stage>| !matches!(stage, Some(Stage::Done(_)));
        let open: Vec<OperationId> = self
            .watching
            .iter()
            .filter(|(_, stage)| open(stage))
            .map(|(operation, _)| *operation)
            .collect();
        open
    }

    fn show_watched(&mut self, operation: OperationId, stage: &Stage) {
        self.watching.insert(operation, Some(stage.clone()));
        self.calls.push(Call::ShowWatched(operation, stage.clone()));
    }
}

/// What every test does first: installs one subscriber for the test binary, so the fields of
/// every logged line are evaluated (a line whose fields are never read counts as uncovered).
pub fn begin() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber");
    });
}

/// Runs a future of the core, none of which waits for anything outside itself.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a future of the core waited"),
    }
}

/// What a test makes a writer do: the attempt for the grants (the effects that start a
/// lease) and the attempt for the finishes (an answer or a refusal), and how many writes
/// it was asked for.
#[derive(Clone)]
pub struct Gate {
    modes: Arc<Mutex<(Attempt, Attempt)>>,
    asked: Arc<AtomicUsize>,
}

impl Gate {
    /// A writer that takes everything until the gate says otherwise, and the gate.
    pub fn new(trace: &Trace) -> (impl Writer + 'static, Self) {
        let gate = Self {
            modes: Arc::new(Mutex::new((Attempt::Taken, Attempt::Taken))),
            asked: Arc::new(AtomicUsize::new(0)),
        };
        let (seen, trace) = (gate.clone(), Arc::clone(trace));
        let writer = move |effect: &Effect, _: &Settles<'_>| {
            seen.asked.fetch_add(1, Ordering::SeqCst);
            let (grants, finishes) = *seen.modes.lock().expect("modes");
            let (kind, attempt) = match effect {
                Effect::Start(start) => (format!("start {}", start.lease), grants),
                Effect::Answer(answer) => (format!("answer {}", answer.lease), finishes),
                Effect::Refuse(refusal) => (format!("refuse {}", refusal.operation), finishes),
                Effect::Commit(_) | Effect::Waiting(_) => ("other".to_owned(), Attempt::Taken),
            };
            trace
                .lock()
                .expect("trace")
                .push(format!("write {kind} {attempt:?}"));
            attempt
        };
        (writer, gate)
    }

    pub fn grants(&self, attempt: Attempt) {
        self.modes.lock().expect("modes").0 = attempt;
    }

    pub fn finishes(&self, attempt: Attempt) {
        self.modes.lock().expect("modes").1 = attempt;
    }

    pub fn all(&self, attempt: Attempt) {
        *self.modes.lock().expect("modes") = (attempt, attempt);
    }

    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

pub fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

pub fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

pub fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64")])
        .expect("a node report")
        .with_drivers(["container"])
}

/// A serving host with `cpus` cores and 64 GiB.
pub fn reported(cpus: u64) -> Reported {
    Reported::serving(Resources::new(cpus * 1_000, 64 * GIB), caps())
}

/// Caller `n`'s request: one core and 1 GiB, any platform; `n` names the action.
pub fn request(n: u64) -> Request {
    Request {
        key: WorkKey::new("main", format!("action-{n}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

/// A core over a new rig, of term 1.
pub fn core() -> Core<Rig> {
    begin();
    Core::new(&Config::new(1), Rig::default())
}

/// A core over a new rig, built from `config`.
pub fn core_by(config: &Config) -> Core<Rig> {
    begin();
    Core::new(config, Rig::default())
}

/// A core over a rig that shares `trace`, whose writer is `writer`.
pub fn gated(trace: &Trace, writer: impl Writer + 'static) -> Core<Rig> {
    gated_by(&Config::new(1), trace, writer)
}

/// [`gated`], built from `config`.
pub fn gated_by(config: &Config, trace: &Trace, writer: impl Writer + 'static) -> Core<Rig> {
    begin();
    let rig = Rig {
        trace: Arc::clone(trace),
        ..Rig::default()
    };
    Core::new(config, rig).with_writer(writer)
}

/// Registers `name` as daemon process `instance`, which names no predecessor, and records
/// the stream as open.
pub fn up(core: &Core<Rig>, now: u64, name: &str, instance: &str) -> Stream {
    up_after(core, now, name, (instance, ""), reported(8))
}

/// Registers `name` as daemon process `instance`, naming `predecessor` (the pair), with
/// what it `reported`.
pub fn up_after(
    core: &Core<Rig>,
    now: u64,
    name: &str,
    (instance, predecessor): (&str, &str),
    reported: Reported,
) -> Stream {
    try_up(core, now, name, (instance, predecessor), reported).expect("registered")
}

/// [`up_after`], for a host the admission may refuse. Every registration of the tests goes
/// through here, so the core's `register` is built once.
pub fn try_up(
    core: &Core<Rig>,
    now: u64,
    name: &str,
    (instance, predecessor): (&str, &str),
    reported: Reported,
) -> Result<Stream, Denied> {
    let who = worker(name);
    let opened = who.clone();
    core.register(
        at(now),
        &who,
        DaemonInstance::new(instance),
        DaemonInstance::new(predecessor),
        reported,
        move |rig, stream| {
            rig.open.insert((opened, stream));
        },
    )
}

/// Resends `name`'s report on `stream`; the rig counts the updates that reach it.
pub fn resend(
    core: &Core<Rig>,
    now: u64,
    (name, stream): (&str, Stream),
    reported: Reported,
) -> Result<(), Denied> {
    core.resize(at(now), &worker(name), stream, reported, |rig| {
        rig.resized += 1;
    })
}

/// Records the caller a submit became.
pub fn enrol(rig: &mut Rig, admitted: &Admitted) {
    rig.callers.insert(admitted.waiter, *admitted);
}

/// Submits `request`, enrolling the caller in the rig.
pub fn submit_request(core: &Core<Rig>, now: u64, request: Request) -> Result<Admitted, NotQueued> {
    core.submit(at(now), request, &now.to_be_bytes(), enrol)
}

/// Marks the stream closed, as the host's connection dropping would.
pub fn close(core: &Core<Rig>, name: &str, stream: Stream) {
    let who = worker(name);
    core.with(|_, rig| rig.open.remove(&(who, stream)));
}

/// Submits request `n`, enrolling the caller in the rig.
pub fn submit(core: &Core<Rig>, now: u64, n: u64) -> Admitted {
    submit_request(core, now, request(n)).expect("submitted")
}

/// Heartbeat `seq` of `name` on `stream`, listing `running`.
pub fn beat(
    core: &Core<Rig>,
    now: u64,
    (name, stream): (&str, Stream),
    seq: u64,
    running: &[LeaseId],
) -> bool {
    let beat = Beat::new(seq, running.to_vec());
    block_on(core.heartbeat(at(now), &worker(name), stream, beat)).expect("admitted")
}

/// Every hook the rig was called through, in order.
pub fn calls(core: &Core<Rig>) -> Vec<Call> {
    core.with(|_, rig| rig.calls.clone())
}

/// The leases the rig was told to start, in order.
pub fn started(core: &Core<Rig>) -> Vec<LeaseId> {
    calls(core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Start { lease, .. } => Some(lease),
            _ => None,
        })
        .collect()
}

/// Whether an operation in `state` has a lease that may still report.
fn open(state: &OpState) -> bool {
    matches!(state, OpState::Leased { .. } | OpState::Running { .. })
}

/// The clock and report hooks the rig was called through, and settles, in order.
pub fn hooks(core: &Core<Rig>) -> Vec<Hook> {
    core.with(|_, rig| rig.hooks.lock().expect("hooks").clone())
}

/// The lines of `trace` so far.
pub fn lines(trace: &Trace) -> Vec<String> {
    trace.lock().expect("trace").clone()
}

pub fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId::new(term, seq)
}

/// What `waiter`, a caller of an operation another frontend runs, was last shown.
pub fn shown_to(core: &Core<Rig>, waiter: WaiterId) -> Option<Stage> {
    core.with(|_, rig| rig.shown.get(&waiter).cloned())
}

/// A log sink a test can read back.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut kept = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        kept.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Runs `run` under a subscriber of its own on this thread, and returns what it logged.
pub fn logged(run: impl FnOnce()) -> String {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    let kept = sink
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    String::from_utf8_lossy(&kept).into_owned()
}
