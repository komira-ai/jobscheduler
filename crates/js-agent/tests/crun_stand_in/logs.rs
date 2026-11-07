//! Live logs of `CrunRuntime` against the stand-in: a run whose start carries a sink is
//! uploaded to an in-memory store by an uploader the test steps (its wake is manual), and
//! nothing the runtime or the lifecycle does waits for it.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Clock, Config, Finished, Lifecycle, MountRoots, Prepared,
    Runtime, ScratchRoot, StateRoot, Status,
};
use js_agent::logs::{HostGrant, Limits, LiveLogs, ManualWake, Settled, Sink, Wake};
use js_agent_wire::{
    BookedResources, Command, HeartbeatReply, JobStartSpec, LogGrant, LogReport, LogSink,
};
use js_logstore::fake::{Action, FakeStore, Policy};
use js_logstore::{Endpoint, JobPrefix, ManualClock, Stream, Transport};

use crate::node::{LONG, Node};

const MIB: u64 = 1024 * 1024;

/// What the stand-in's `*-big` writes to each stream.
const BIG: u64 = 10 * MIB;

/// The capture file cap of a run with a sink in these tests.
const CAP: u64 = 2 * MIB;

fn endpoint() -> Endpoint {
    Endpoint::parse("http://127.0.0.1:9000", "test-1", "logs", true).expect("an endpoint")
}

fn sink(run_id: &str) -> Sink {
    Sink::new(endpoint(), JobPrefix::new("js", run_id).expect("a prefix"))
}

/// A store, the host's credential, and the settings of a runtime that uploads to it, whose
/// uploaders' wakes are kept in `wakes` in the order they were made.
struct Live {
    store: FakeStore,
    logs: LiveLogs,
    wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
}

impl Live {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new(
            UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        ));
        let store = FakeStore::new(endpoint(), clock.clone());
        let credential = store.issue(Policy {
            actions: vec![Action::Put, Action::Delete],
            prefix: "js/".to_owned(),
        });
        let grant = Arc::new(HostGrant::new(clock.clone()));
        let expires = credential
            .expires_at
            .duration_since(UNIX_EPOCH)
            .expect("later");
        grant.replace(&LogGrant {
            access_key_id: credential.access_key_id.clone(),
            secret_access_key: credential.secret_access_key.clone(),
            session_token: credential.session_token.clone(),
            expires_at_ms: u64::try_from(expires.as_millis()).expect("ms"),
        });
        let wakes: Arc<Mutex<Vec<Arc<ManualWake>>>> = Arc::default();
        let made = wakes.clone();
        let logs = LiveLogs {
            file_cap: CAP,
            grant,
            transport: Arc::new(store.clone()),
            clock,
            wakes: Arc::new(move || {
                let wake = ManualWake::new();
                made.lock().expect("wakes").push(wake.clone());
                wake as Arc<dyn Wake>
            }),
            limits: Limits::default(),
        };
        Self { store, logs, wakes }
    }

    /// The same settings over `transport`.
    fn over(mut self, transport: Arc<dyn Transport>) -> Self {
        self.logs.transport = transport;
        self
    }

    fn made(&self) -> usize {
        self.wakes.lock().expect("wakes").len()
    }

    fn wake(&self, index: usize) -> Arc<ManualWake> {
        self.wakes.lock().expect("wakes")[index].clone()
    }
}

/// Steps the uploader of `wake` until it has ended.
fn run_out(wake: &ManualWake) {
    while wake.step() != Settled::Ended {}
}

/// Proves a store that holds every put holds nothing of the runtime: with the uploader of a
/// run that wrote 10 MiB a stream inside a put the store will not answer, the read of the
/// run's end, its report, its tail, the listing and the removal of its lease all return.
/// Once the store answers, the upload ends with the chunks up to the cap and the gap marker
/// after them, read from files that were removed. Catches a runtime call that waits on the
/// uploader.
#[test]
fn a_store_that_hangs_does_not_hold_the_runtime() {
    let node = Node::new("logs-hang");
    let live = Live::new();
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j1-big", 60_000);
    spec.log_sink = Some(sink("j1-big"));
    live.store.block_puts();
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    let wake = live.wake(0);
    let stepping = std::thread::spawn(move || run_out(&wake));
    live.store.wait_for_held_puts(1);

    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    let report = LogReport {
        stdout_bytes: BIG,
        stderr_bytes: BIG,
        stdout_dropped: BIG - CAP,
        stderr_dropped: BIG - CAP,
    };
    assert_eq!(runtime.logs_report(&id), Some(report));
    assert!(runtime.log_tail(&id).expect("a tail").is_some());
    assert_eq!(runtime.list_labelled().expect("lists").len(), 1);
    runtime.remove(&id).expect("removed");
    assert!(
        !node.lease_of(&id.0).exists(),
        "the lease directory is gone"
    );

    live.store.release_puts();
    stepping.join().expect("the uploader ended");
    let objects = live.store.objects();
    let prefix = JobPrefix::new("js", "j1-big").expect("a prefix");
    for stream in [Stream::Stdout, Stream::Stderr] {
        let chunks: u64 = (0..CAP / MIB)
            .map(|n| objects[&prefix.chunk_key(stream, n * MIB)].len() as u64)
            .sum();
        assert_eq!(chunks, CAP, "{stream:?}");
        let marker = format!("{}.gap", prefix.chunk_key(stream, CAP));
        assert_eq!(
            objects[&marker],
            format!("{{\"length\":{}}}", BIG - CAP).as_bytes()
        );
        assert!(
            !objects.contains_key(&prefix.final_key(stream)),
            "no final over a gap"
        );
    }
}

/// Proves the report of a run with a sink: what each stream printed, and none dropped under
/// the cap. Catches lengths taken from the file or the tail rather than the capture's count.
#[test]
fn the_result_reports_lengths_and_dropped_bytes() {
    let node = Node::new("logs-lengths");
    let live = Live::new();
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j2-say", 60_000);
    spec.log_sink = Some(sink("j2-say"));
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    let report = LogReport {
        stdout_bytes: 10,
        stderr_bytes: 10,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(runtime.logs_report(&id), Some(report));
    run_out(&live.wake(0));
    runtime.remove(&id).expect("removed");
    let prefix = JobPrefix::new("js", "j2-say").expect("a prefix");
    let objects = live.store.objects();
    assert_eq!(objects[&prefix.final_key(Stream::Stdout)], b"to-stdout\n");
    assert_eq!(objects[&prefix.final_key(Stream::Stderr)], b"to-stderr\n");
    assert_eq!(objects.len(), 2, "the chunks were deleted: {objects:?}");
}

/// Proves that what has no sink is as it was: a run with no sink on a runtime that uploads,
/// and a run with a sink on a runtime that does not, make no uploader (no wake, no request),
/// report no `logs`, and keep the capture's 1 MiB file. Catches an uploader, a larger cap or
/// a report where no live logs were asked for.
#[test]
fn without_a_sink_nothing_changes() {
    let node = Node::new("logs-none");
    let live = Live::new();
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let id = runtime
        .create_and_start(&node.job("j3-big", 60_000))
        .expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(runtime.logs_report(&id), None);
    let stdout = node.lease_of(&id.0).join("stdout");
    assert_eq!(std::fs::metadata(stdout).expect("the file").len(), MIB);
    runtime.remove(&id).expect("removed");
    drop(runtime);

    let mut plain = node.open();
    let mut spec = node.job("j4-big", 60_000);
    spec.log_sink = Some(sink("j4-big"));
    let id = plain.create_and_start(&spec).expect("started");
    plain.wait_ended(&id);
    assert!(matches!(plain.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(plain.logs_report(&id), None);
    let stdout = node.lease_of(&id.0).join("stdout");
    assert_eq!(std::fs::metadata(stdout).expect("the file").len(), MIB);
    plain.remove(&id).expect("removed");

    assert_eq!(live.made(), 0, "no uploader was started");
    assert!(live.store.operations().is_empty());
}

/// A transport that panics on its first request.
struct Panicking;

impl Transport for Panicking {
    fn send(
        &self,
        _: js_logstore::HttpRequest,
    ) -> Result<js_logstore::HttpResponse, js_logstore::TransportError> {
        panic!("a transport fault")
    }
}

/// Proves a panic in the uploader stays there: its transport panics on the first request,
/// made at the run's end; the uploader's thread ends, and the run's end, tail, report and
/// removal are as they would be. Catches a panic that reaches the runtime or the run.
#[test]
fn a_panic_in_the_uploader_does_not_touch_the_job() {
    let node = Node::new("logs-panic");
    let live = Live::new().over(Arc::new(Panicking));
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j5-say", 60_000);
    spec.log_sink = Some(sink("j5-say"));
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(_))));
    run_out(&live.wake(0));
    let tail = runtime.log_tail(&id).expect("a tail").expect("there");
    assert_eq!(tail.stdout, "to-stdout\n");
    assert_eq!(runtime.logs_report(&id).map(|r| r.stdout_bytes), Some(10));
    runtime.remove(&id).expect("removed");
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

struct Pass;

impl CheckHook for Pass {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        Ok(())
    }
}

struct Still;

impl Clock for Still {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn private(node: &Node, name: &str) -> std::path::PathBuf {
    let path = node.dir.join(name);
    std::fs::create_dir_all(&path).expect("a directory");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("private");
    path
}

/// The job start of `job_id` on the node's image, uploading to the test's store.
fn job_start(node: &Node, job_id: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.to_owned(),
        allocation_id: format!("alloc-{job_id}"),
        image: node.image(),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: Some(LogSink {
            endpoint: "http://127.0.0.1:9000".to_owned(),
            region: "test-1".to_owned(),
            bucket: "logs".to_owned(),
            prefix: format!("js/{job_id}/"),
            path_style: true,
        }),
        kind: Default::default(),
        isolation: Default::default(),
    }
}

/// Proves the result and the cleanup never wait for the upload: a job with a sink writes
/// 10 MiB a stream under the lifecycle while the store holds every put; its listing carries
/// the result with its tail and `logs`, and its lease is removed, while the upload is still
/// held. Catches a result or a cleanup that waits for logging.
#[test]
fn the_result_is_reported_before_the_final_is_written() {
    let node = Node::new("logs-result-first");
    let live = Live::new();
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: BTreeSet::new(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open(&private(&node, "life-scratch")).expect("scratch"),
        files: None,
        state: StateRoot::open(&private(&node, "life-state")).expect("state"),
        stop_grace: LONG,
        bundle: None,
        quota: None,
    };
    let mut runtime = node.open().with_live_logs(live.logs.clone());
    let _ = runtime.prepare(&node.image(), None);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);
    live.store.block_puts();
    let (mut life, _) =
        Lifecycle::adopt(config, Box::new(runtime), Box::new(Pass), Box::new(Still))
            .expect("adopts");
    life.apply(&HeartbeatReply {
        commands: vec![Command::JobStart(Box::new(job_start(&node, "j6-big")))],
        ..HeartbeatReply::default()
    });
    let wake = live.wake(0);
    let stepping = std::thread::spawn(move || run_out(&wake));
    live.store.wait_for_held_puts(1);
    let result = loop {
        life.step();
        let listed = life.held_jobs();
        if let Some(result) = listed[0].result.clone() {
            break result;
        }
    };
    assert!(result.log_tail.is_some());
    assert_eq!(
        result.logs,
        Some(LogReport {
            stdout_bytes: BIG,
            stderr_bytes: BIG,
            stdout_dropped: BIG - CAP,
            stderr_dropped: BIG - CAP,
        })
    );
    while std::fs::read_dir(&node.scratch)
        .expect("the store")
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().ends_with("j6-big"))
    {
        life.step();
    }
    assert!(live.store.objects().is_empty(), "the upload is still held");
    live.store.release_puts();
    stepping.join().expect("the uploader ended");
}
