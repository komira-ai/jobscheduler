//! The durable core against the core-driver conformance suite (`js_conformance::core`).
//!
//! [`Harness`] drives a [`Core`] opened over `js-store` on the in-memory state store, behind
//! a wrapper that refuses the writes of a lease or a result on request ([`Flaky`]). A crash
//! and restart drops the core and opens a new one over the same store, the same row codec
//! and the same farm time, so the lease term (the store's epoch) and the blocks of lease
//! sequences are the store's.
//!
//! The harness polls the store's intake at every advance, as a server does, and its core
//! requeues a lease whose node has no session at the second look of the orphan sweep: a start
//! takes up no leased operation (its node may be connected to another frontend), so the lease
//! of a node that never comes back is given up by that sweep, not by the start.
//!
//! The suite's reference core stays as it is. Of its eight defects, six have a broken
//! variant of this harness, not of the core (the core has no switch to break): a writer
//! that takes a grant without writing it, a writer that deletes a finished operation's lease
//! row and leaves its `op/` row, a store that forgets the lease sequence counter, a host
//! that sends a replaced stream's heartbeat on the newest stream, a store whose refusal
//! reads as final, and a store whose scan of the lease rows comes back empty. Each turns
//! the scenario the defect belongs to red.
//! The other two (a restart that forgets the queue when no worker had registered, and one
//! that forgets it after a refused grant) are model-only: they break a rule of the model's
//! boot, and the core is checked against their scenarios by passing them.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_caps::NodeCaps;
use js_conformance::Verdict;
use js_conformance::core::{
    self, Answered, CoreHarness, Observed, Released, Scenario, Stream as HarnessStream,
    UNPLACEABLE_JOB,
};
use js_sched::DaemonInstance;
use js_server_core::{
    Adapter, Attempt, Beat, Config, Core, Delivery, Report, Reported, Settles, Stage, Standing,
    Storage, Stream, View,
};
use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{Ending, RowCodec, Value, keys};
use js_types::sched::{
    Answer, Effect, FarmTime, LeaseGrant, LeaseId, LeaseKind, NodePhase, OperationId, Outcome, Qos,
    Refusal, Resources, ResultRef, StartLease, WaiterId, WorkKey, WorkerId,
};

const TERM: u64 = 1;
const GIB: u64 = 1 << 30;

/// What a store gets wrong in a broken variant of the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lie {
    /// Nothing: the store is as it is.
    None,
    /// A refusal is reported as one no retry can overcome.
    RefusalsAreFinal,
    /// The lease sequence counter reads as zero.
    ForgetsLeaseCounter,
    /// A scan of the lease rows finds none.
    HidesLeases,
}

/// The in-memory store, refusing on request the writes that touch `lease/` or `done/` rows
/// (a submission touches neither), and lying in one way when a broken variant asks.
struct Flaky {
    fake: Mutex<Fake>,
    clock: Arc<ManualClock>,
    refusals: AtomicU32,
    lie: Mutex<Lie>,
}

impl Flaky {
    fn new(clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            fake: Mutex::new(Fake::with_flaw(clock.clone(), Flaw::None)),
            clock,
            refusals: AtomicU32::new(0),
            lie: Mutex::new(Lie::None),
        })
    }

    fn fake(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.fake.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lie(&self) -> Lie {
        *self.lie.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tell(&self, lie: Lie) {
        *self.lie.lock().unwrap_or_else(PoisonError::into_inner) = lie;
    }

    /// Every write of a lease or a result fails until `advances` advances of the clock have
    /// passed (`u32::MAX`: until asked otherwise).
    ///
    /// The suite counts writes, for a reference core that places work at a tick. This core
    /// places work as soon as it is submitted, as the farm's does, so it tries the grant
    /// once before the advance the scenario waits for: a count of writes would let the
    /// retry at that advance through. A refusal per advance holds the scenario to what it
    /// is about, that nothing is released while the store refuses, and that what it held is
    /// written in order once it takes writes again.
    fn refuse(&self, advances: u32) {
        self.refusals.store(advances, Ordering::SeqCst);
    }

    /// An advance of the clock has passed.
    fn advanced(&self) {
        let pending = self.refusals.load(Ordering::SeqCst);
        if pending != 0 && pending != u32::MAX {
            self.refusals.store(pending - 1, Ordering::SeqCst);
        }
    }

    /// A crash and reopen of the store: what was synced survives.
    fn restart(&self) {
        let mut fake = self.fake();
        let empty = Fake::with_flaw(self.clock.clone(), Flaw::None);
        *fake = std::mem::replace(&mut *fake, empty).restart();
    }

    /// The rows under `prefix` as they are, whatever the store lies about.
    fn truth(&self, prefix: &str) -> Vec<(String, Versioned)> {
        self.fake().scan_prefix(prefix).expect("the fake scans")
    }

    /// Whether `txn` writes a `lease/` or a `done/` row.
    fn touches_control_rows(txn: &Txn) -> bool {
        txn.ops().iter().any(|op| match op {
            Op::Put { key, .. } | Op::Delete { key, .. } => {
                key.starts_with("lease/") || key.starts_with("done/")
            }
            Op::Check { .. } | Op::Next { .. } => false,
        })
    }

    /// Whether writes of leases and results are refused now.
    fn refusing(&self) -> bool {
        self.refusals.load(Ordering::SeqCst) != 0
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if batch.iter().any(Self::touches_control_rows) && self.refusing() {
            return Err(match self.lie() {
                Lie::RefusalsAreFinal => StoreError::Fenced,
                _ => StoreError::Failed("the store refuses".to_owned()),
            });
        }
        self.fake().apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let mut row = self.fake().get(key)?;
        if key == keys::NEXT_SEQ && self.lie() == Lie::ForgetsLeaseCounter {
            row = row.map(|found| Versioned {
                value: 0u64.to_be_bytes().to_vec(),
                ..found
            });
        }
        Ok(row)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if prefix == keys::LEASE && self.lie() == Lie::HidesLeases {
            return Ok(Vec::new());
        }
        self.fake().scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.fake().scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fake().changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fake().reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.fake().commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.fake().now()
    }
}

/// What the harness sees of the core: the adapter. It records what leaves the core into the
/// [`Observed`] the harness keeps across restarts, in the harness's own numbers.
struct Probe {
    seen: Arc<Mutex<Observed>>,
    /// The harness's stream for each stream the core numbered.
    streams: BTreeMap<Stream, HarnessStream>,
    /// The harness's caller for each caller the core numbered.
    callers: BTreeMap<WaiterId, WaiterId>,
}

impl Probe {
    fn seen(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn callers_of(&self, waiters: &[WaiterId]) -> Vec<WaiterId> {
        let known = |waiter: &WaiterId| self.callers.get(waiter).copied();
        waiters.iter().filter_map(known).collect()
    }
}

impl Adapter for Probe {
    fn stream_open(&self, _: &WorkerId, _: Stream) -> bool {
        true
    }

    fn name(&self, _: &View<'_>, operation: OperationId) -> String {
        operation.0.to_string()
    }

    fn phased(&mut self, _: &WorkerId, _: &NodePhase, _: &NodePhase) {}

    fn offer(&mut self, _: &View<'_>, _: &LeaseGrant) {}

    fn start(&mut self, view: &View<'_>, start: &StartLease) {
        let core_stream = view.link(&start.worker).map(|link| link.stream);
        let stream = core_stream.and_then(|stream| self.streams.get(&stream).copied());
        self.seen().released.push(Released {
            worker: start.worker.to_string(),
            stream: stream.unwrap_or_default(),
            lease: start.lease,
            operation: start.operation,
        });
    }

    fn cancel(&mut self, _: &View<'_>, _: &WorkerId, _: LeaseId) {}

    fn show(&mut self, _: &View<'_>, _: OperationId, _: &Stage) {}

    fn refuse(&mut self, _: &View<'_>, refusal: &Refusal) {
        let waiters = self.callers_of(&refusal.waiters);
        self.seen().answered.push(Answered {
            operation: refusal.operation,
            waiters,
        });
    }

    fn requeued(&mut self, _: &js_sched::Requeue) {}

    fn settle(&mut self, _: &View<'_>, answer: &Answer, _: Option<&WorkerId>) -> Delivery {
        let waiters = self.callers_of(&answer.waiters);
        self.seen().answered.push(Answered {
            operation: answer.operation,
            waiters,
        });
        Box::pin(std::future::ready(()))
    }

    fn forget_dropped(&mut self, _: &dyn Fn(OperationId) -> bool) {}

    fn forget_parcel(&mut self, _: LeaseId) {}

    fn retain_parcels(&mut self, _: &dyn Fn(LeaseId) -> bool) {}

    fn ending(&self, effect: &Effect, retention: Duration) -> Option<Ending> {
        let done: &[u8] = match effect {
            Effect::Answer(_) => b"answered",
            Effect::Refuse(_) => b"refused",
            _ => return None,
        };
        Some(Ending::new(done.to_vec(), retention))
    }

    fn adopted(&mut self, _: OperationId, waiter: WaiterId, parcel: Option<&[u8]>) {
        let caller = parcel
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_be_bytes);
        if let Some(caller) = caller {
            self.callers.insert(waiter, WaiterId(caller));
        }
    }

    fn waiting_among(&self, _: &[WaiterId]) -> Vec<WaiterId> {
        Vec::new()
    }

    fn show_callers(&mut self, _: &[WaiterId], _: &Stage) {}

    fn keep_finished(&mut self, _: OperationId, _: Vec<WaiterId>) {}

    fn followed(&mut self, _: OperationId, _: &[u8]) {}

    fn watched(&mut self) -> Vec<OperationId> {
        Vec::new()
    }

    fn show_watched(&mut self, _: OperationId, _: &Stage) {}
}

/// How a broken variant of the harness differs from the correct one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    /// Nothing.
    Nothing,
    /// A grant is taken by the writer and not written.
    TakesGrantsUnwritten,
    /// A result is written as its lease row gone and its operation queued again.
    LeavesFinishedOperations,
    /// A heartbeat of a replaced stream is sent on the worker's newest.
    FeedsStaleHeartbeats,
    /// The store lies in this way.
    Lies(Lie),
}

/// The durable core behind the suite's harness trait.
struct Harness {
    clock: Arc<ManualClock>,
    store: Arc<Flaky>,
    codec: Arc<TestCodec>,
    seen: Arc<Mutex<Observed>>,
    broken: Break,
    core: Option<Core<Probe>>,
    now: u64,
    /// The core's stream for each stream the harness numbered, for the current core.
    streams: BTreeMap<HarnessStream, Stream>,
    /// The newest stream of each worker, as the harness numbered it.
    newest: BTreeMap<String, HarnessStream>,
    registrations: u64,
    beats: u64,
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a future of the core waited"),
    }
}

/// The request a job number stands for: one core, one GiB, a container; different numbers
/// are different work, and the unplaceable job asks for more cores than any worker has.
fn request(job: u8) -> js_sched::Request {
    let cores = if job == UNPLACEABLE_JOB {
        1_000_000
    } else {
        1_000
    };
    js_sched::Request {
        key: WorkKey::new("job", format!("job-{job}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(cores, GIB),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn reported() -> Reported {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("a node report")
        .with_drivers(["container"]);
    Reported::serving(Resources::new(4_000, 8 * GIB), caps)
}

impl Harness {
    fn new() -> Self {
        Self::with(Break::Nothing)
    }

    fn with(broken: Break) -> Self {
        let clock = Arc::new(ManualClock::default());
        let store = Flaky::new(clock.clone());
        if let Break::Lies(lie) = broken {
            store.tell(lie);
        }
        let mut harness = Self {
            clock,
            store,
            codec: Arc::new(TestCodec::new()),
            seen: Arc::new(Mutex::new(Observed::default())),
            broken,
            core: None,
            now: 0,
            streams: BTreeMap::new(),
            newest: BTreeMap::new(),
            registrations: 0,
            beats: 0,
        };
        harness.boot();
        harness
    }

    fn at(&self) -> FarmTime {
        FarmTime::from_millis(self.now)
    }

    fn core(&self) -> &Core<Probe> {
        self.core.as_ref().expect("the core is running")
    }

    /// Opens a core over the store, at the same term.
    fn boot(&mut self) {
        let probe = Probe {
            seen: Arc::clone(&self.seen),
            streams: BTreeMap::new(),
            callers: BTreeMap::new(),
        };
        let storage = Storage::new(self.store.clone(), self.codec.clone(), Arc::new(TestExt));
        // The suite's scenario gives an unclaimed lease up once its node has had no session for
        // the grace: the sweep acts on its second look, with no further wait.
        let config = Config::new(TERM).with_orphan_wait(Duration::ZERO);
        let core = Core::open(&config, probe, storage, self.at()).expect("opened");
        self.core = Some(match self.broken {
            Break::TakesGrantsUnwritten => core.with_writer(takes_grants_unwritten),
            Break::LeavesFinishedOperations => {
                core.with_writer(leaves_operations(&self.store, &self.codec))
            }
            _ => core,
        });
    }

    fn core_stream(&self, stream: HarnessStream) -> Option<Stream> {
        self.streams.get(&stream).copied()
    }
}

/// A writer that takes a grant and writes nothing.
fn takes_grants_unwritten(_: &Effect, _: &Settles<'_>) -> Attempt {
    Attempt::Taken
}

/// A writer that writes what the core's does, except that a result is written as the lease
/// row gone and the operation queued again: the operation stays unfinished in the store.
fn leaves_operations(
    store: &Arc<Flaky>,
    codec: &Arc<TestCodec>,
) -> impl FnMut(&Effect, &Settles<'_>) -> Attempt + Send + 'static {
    let store = Arc::clone(store);
    let rows = js_store::Rows::open(
        store.clone(),
        js_store::EpochPolicy::Keep,
        TERM,
        codec.clone(),
        Arc::new(TestExt),
    )
    .expect("a handle for the broken writer")
    .0;
    move |effect: &Effect, settles: &Settles<'_>| match (effect, settles) {
        (Effect::Start(_), Settles::Grant(grant)) => attempt_of(rows.granted(grant)),
        (Effect::Refuse(refusal), Settles::Ending(ending)) => {
            attempt_of(rows.refused(refusal.operation, ending))
        }
        (Effect::Answer(answer), _) => {
            let undone = Txn::new().delete(keys::lease(answer.lease), Cond::Any).put(
                keys::queue(answer.operation),
                Vec::new(),
                Cond::Any,
            );
            attempt_of(store.txn(undone).map(|_| ()).map_err(Into::into))
        }
        _ => Attempt::Taken,
    }
}

fn attempt_of(written: Result<(), js_store::StateError>) -> Attempt {
    match written {
        Ok(()) => Attempt::Taken,
        Err(js_store::StateError::Store(_)) => Attempt::Transient,
        Err(_) => Attempt::Dropped,
    }
}

impl CoreHarness for Harness {
    fn submit(&mut self, waiter: u64, job: u8) -> OperationId {
        let caller = WaiterId(waiter);
        let enrol = |probe: &mut Probe, admitted: &js_server_core::Admitted| {
            probe.callers.insert(admitted.waiter, caller);
        };
        let parcel = waiter.to_be_bytes();
        let admitted = self.core().submit(self.at(), request(job), &parcel, enrol);
        admitted.expect("the store takes a submission").operation
    }

    fn worker_up(&mut self, worker: &str) -> HarnessStream {
        self.registrations += 1;
        let stream = HarnessStream::new(self.registrations);
        let name = WorkerId::new(worker);
        let bind = move |probe: &mut Probe, core_stream: Stream| {
            probe.streams.insert(core_stream, stream);
        };
        let instance = DaemonInstance::new("daemon");
        let predecessor = DaemonInstance::new("");
        let up = self
            .core()
            .register(self.at(), &name, instance, predecessor, reported(), bind);
        let core_stream = up.expect("the host is heard");
        self.streams.insert(stream, core_stream);
        self.newest.insert(worker.to_owned(), stream);
        stream
    }

    fn heartbeat(&mut self, worker: &str, stream: HarnessStream, running: &[LeaseId]) {
        let newest = self.newest.get(worker).copied();
        let sent_on = match self.broken {
            Break::FeedsStaleHeartbeats => newest,
            _ => Some(stream),
        };
        let Some(core_stream) = sent_on.and_then(|stream| self.core_stream(stream)) else {
            return;
        };
        self.beats += 1;
        let beat = Beat::new(self.beats, running.to_vec());
        let name = WorkerId::new(worker);
        let heard = self.core().heartbeat(self.at(), &name, core_stream, beat);
        block_on(heard).expect("the host is heard");
    }

    fn report(&mut self, operation: OperationId, lease: LeaseId) {
        let released = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let holder = released.released.iter().find(|start| start.lease == lease);
        let Some(worker) = holder.map(|start| WorkerId::new(&start.worker)) else {
            return;
        };
        drop(released);
        let standing = self.core().standing(&worker, lease);
        if !matches!(standing, Ok(Standing::Current(_))) {
            return;
        }
        let action_result = ResultRef::new([7; 32], 0);
        let report = Report {
            operation,
            lease,
            outcome: Outcome::Completed { action_result },
        };
        let taken = self.core().report(self.at(), &worker, report, |_| {});
        block_on(taken);
    }

    fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.now += u64::try_from(by.as_millis()).expect("small");
        // A server polls the store for what other frontends did between ticks; a lease whose
        // node never comes back is requeued by that poll's orphan sweep.
        self.core().intake(self.at());
        block_on(self.core().tick(self.at()));
        self.store.advanced();
    }

    fn refuse_writes(&mut self, count: u32) {
        self.store.refuse(count);
    }

    fn crash_and_restart(&mut self) {
        self.core = None;
        self.store.restart();
        self.store.refuse(0);
        self.streams.clear();
        self.newest.clear();
        self.boot();
    }

    fn observed(&self) -> Observed {
        let mut rows = self.store.truth(keys::LEASE);
        rows.sort_by_key(|(_, row)| row.version);
        let lease_of = |(key, row): (String, Versioned)| match self.codec.read(&key, &row.value) {
            Ok(Value::Lease(grant)) => Some(grant.lease),
            _ => None,
        };
        let stored_leases = rows.into_iter().filter_map(lease_of).collect();
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        Observed {
            stored_leases,
            ..seen.clone()
        }
    }
}

/// What a report says went wrong, for the assertion message.
fn failures(report: &core::CoreReport) -> Vec<(Scenario, Verdict)> {
    let failed = |(_, verdict): &&(Scenario, Verdict)| matches!(verdict, Verdict::Failed(_));
    report.results.iter().filter(failed).cloned().collect()
}

/// Asserts that the durable core passes the nine scenarios of the core-driver suite, over
/// the in-memory store behind the wrapper that refuses lease and result writes. Catches: a
/// `Start` before its grant is in the store, an operation answered twice across a restart,
/// a lease id handed out twice, a replaced stream's heartbeat fed, a refused write that is
/// lost, a recovered lease given up before its grace, and a lease given up for silence while
/// the store is down.
#[test]
fn the_durable_core_passes_the_core_driver_suite() {
    let report = core::run(Harness::new);
    assert_eq!(failures(&report), []);
    assert_eq!(report.results.len(), Scenario::ALL.len());
}

fn red_on(broken: Break, scenario: Scenario) {
    let report = core::run(move || Harness::with(broken));
    let failed: Vec<Scenario> = failures(&report).into_iter().map(|(s, _)| s).collect();
    // The broken variants break the core's surroundings, not one line of a model, so other
    // scenarios that lean on the same rule may fail with it.
    assert!(
        failed.contains(&scenario),
        "{broken:?} left {scenario:?} green; failed: {failed:?}"
    );
}

/// Asserts that a writer that takes a grant and does not write it turns `StartWaitsForCommit`
/// red: the `Start` is released with no lease in the store. (Reference defect:
/// `StartBeforeCommit`.)
#[test]
fn a_start_released_before_its_grant_is_written_fails_the_suite() {
    red_on(Break::TakesGrantsUnwritten, Scenario::StartWaitsForCommit);
}

/// Asserts that a writer that leaves a finished operation unfinished in the store (its lease
/// row gone, its queue row back) turns `AnsweredOnceAcrossRestart` red: the restart takes the
/// operation up again and answers it a second time. (Reference defect: `AnswerTwice`.)
#[test]
fn a_finished_operation_left_in_the_store_fails_the_suite() {
    red_on(
        Break::LeavesFinishedOperations,
        Scenario::AnsweredOnceAcrossRestart,
    );
}

/// Asserts that a store that forgets the lease sequence counter turns `LeaseIdsNeverRepeat`
/// red: a restart numbers leases from the start of a block again. (Reference defect:
/// `ReusesSeqAfterRestart`.)
#[test]
fn a_restart_that_forgets_the_lease_counter_fails_the_suite() {
    red_on(
        Break::Lies(Lie::ForgetsLeaseCounter),
        Scenario::LeaseIdsNeverRepeat,
    );
}

/// Asserts that a host that sends a replaced stream's heartbeat as the newest's turns
/// `StaleHeartbeatIsNotFed` red: the core, which hears only the newest stream, takes it and
/// gives the lease up. (Reference defect: `FeedsStaleHeartbeat`.)
#[test]
fn a_replaced_streams_heartbeat_fed_to_the_core_fails_the_suite() {
    red_on(
        Break::FeedsStaleHeartbeats,
        Scenario::StaleHeartbeatIsNotFed,
    );
}

/// Asserts that a store whose refusal reads as one no retry can overcome turns
/// `RefusedWriteIsRetriedInOrder` red: the core drops the record, as it must for a write that
/// cannot succeed. (Reference defect: `DropsUnwrittenOnStoreError`.)
#[test]
fn a_refused_write_reported_as_final_fails_the_suite() {
    red_on(
        Break::Lies(Lie::RefusalsAreFinal),
        Scenario::RefusedWriteIsRetriedInOrder,
    );
}

/// Asserts that a store whose scan of the lease rows finds none turns
/// `RecoveredLeaseWaitsForItsWorker` red: the restart takes every operation as queued and
/// places it at once. (Reference defect: `RecoversWithoutGrace`.)
#[test]
fn a_restart_that_forgets_which_operations_hold_a_lease_fails_the_suite() {
    red_on(
        Break::Lies(Lie::HidesLeases),
        Scenario::RecoveredLeaseWaitsForItsWorker,
    );
}

/// Asserts that an operation that is queued when the process dies, for want of a worker or
/// because the store refused its grant, runs once after the restart and is answered once. The
/// suite's scenarios cannot say which operations are queued at a crash for a core that places
/// work as it is submitted; this one does. Catches: a queued operation the restart forgets,
/// or one it starts twice.
#[test]
fn an_operation_queued_when_the_process_dies_runs_once_after_it() {
    for refused in [false, true] {
        let mut harness = Harness::new();
        if refused {
            harness.worker_up("node-a");
            harness.refuse_writes(u32::MAX);
        }
        let operation = harness.submit(1, 1);
        assert!(
            harness.observed().released.is_empty(),
            "queued at the crash"
        );

        harness.crash_and_restart();
        harness.worker_up("node-a");
        harness.advance(Duration::from_secs(1));

        let released = harness.observed().released;
        assert_eq!(released.len(), 1, "refused: {refused}");
        assert_eq!(released[0].operation, operation);
        harness.report(operation, released[0].lease);
        harness.advance(Duration::from_secs(1));
        let answered = harness.observed().answered;
        assert_eq!(answered.len(), 1, "refused: {refused}");
    }
}
