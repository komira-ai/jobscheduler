//! A restart of an agent that uploads live logs, over the stand-in: the first runtime starts a
//! run with a sink, whose uploader keeps its progress record in the lease directory; the test
//! drops that runtime (the agent's restart) and opens another, which adopts the run and, once
//! it is given its live logs, makes the run's uploader from the record. The uploaders' wakes
//! are manual and their clock is one the test moves; nothing waits for time.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use js_agent::crun::CrunRuntime;
use js_agent::lifecycle::{
    Capacity, CheckFinding, CheckHook, Clock, Config, Finished, Lifecycle, MountRoots, RunHandle,
    RunSpec, Runtime, ScratchRoot, StateRoot, Status,
};
use js_agent::logs::{HostGrant, LOG_FLUSH, Limits, LiveLogs, ManualWake, Settled, Sink, Wake};
use js_agent_wire::{LogGrant, LogReport};
use js_logstore::fake::{Action, FakeStore, Policy};
use js_logstore::{Endpoint, JobPrefix, ManualClock, Stream};
use serde_json::json;

use crate::node::{LONG, Node, PID};

/// The record's name in the lease directory.
const RECORD: &str = "logs-upload.json";

fn endpoint() -> Endpoint {
    Endpoint::parse("http://127.0.0.1:9000", "test-1", "logs", true).expect("an endpoint")
}

fn prefix(run_id: &str) -> JobPrefix {
    JobPrefix::new("js", run_id).expect("a prefix")
}

fn sink(run_id: &str) -> Sink {
    Sink::new(endpoint(), prefix(run_id))
}

/// The store, the uploaders' clock and the host's credential of one agent process, and the
/// settings of a runtime that uploads to the store, whose uploaders' wakes are kept in the
/// order they were made.
struct Live {
    store: FakeStore,
    clock: Arc<ManualClock>,
    logs: LiveLogs,
    wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
}

impl Live {
    /// The first agent's: a new store and clock, and a credential held.
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new(
            UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        ));
        let store = FakeStore::new(endpoint(), clock.clone());
        Self::over(store, clock)
    }

    /// The agent's after a restart: the same store and clock, a new credential cell, and the
    /// grant its first heartbeat's reply brings.
    fn restarted(&self) -> Self {
        Self::over(self.store.clone(), self.clock.clone())
    }

    fn over(store: FakeStore, clock: Arc<ManualClock>) -> Self {
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
            file_cap: 2 << 20,
            grant,
            transport: Arc::new(store.clone()),
            clock: clock.clone(),
            wakes: Arc::new(move || {
                let wake = ManualWake::new();
                made.lock().expect("wakes").push(wake.clone());
                wake as Arc<dyn Wake>
            }),
            limits: Limits::default(),
        };
        Self {
            store,
            clock,
            logs,
            wakes,
        }
    }

    fn made(&self) -> usize {
        self.wakes.lock().expect("wakes").len()
    }

    fn wake(&self, index: usize) -> Arc<ManualWake> {
        self.wakes.lock().expect("wakes")[index].clone()
    }

    /// Lets the uploader of `wake`, whose first step is over, take one step after the clock
    /// moved by `LOG_FLUSH`.
    fn flush(&self, wake: &ManualWake) -> Settled {
        self.clock.advance(LOG_FLUSH);
        wake.step()
    }
}

/// Steps the uploader of `wake`, its first step not yet waited for, until it has ended.
fn run_out(wake: &ManualWake) {
    while wake.step() != Settled::Ended {}
}

/// Stages `spec`, starts it, waits for its script to say it is up, and puts its init into its
/// cgroup: a run a restart can adopt.
fn held(node: &Node, runtime: &mut CrunRuntime, spec: &RunSpec) -> RunHandle {
    let id = runtime.stage(spec).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    node.place(&id.0, PID);
    id
}

/// Appends `bytes` to the file `path`, as the capture does.
fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).expect("open");
    file.write_all(bytes).expect("append");
}

/// Stops the held run `id` and removes its lease.
fn finish(runtime: &mut CrunRuntime, id: &RunHandle) {
    runtime.stop(id, LONG).expect("stopped");
    assert!(matches!(runtime.poll(id), Ok(Status::Exited(_))));
    runtime.remove(id).expect("removed");
}

/// A writer that keeps what is logged.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lines").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lines")).into_owned()
    }

    /// Collects every warning logged on this thread.
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        guard
    }
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

/// A lifecycle over `runtime` on `node`.
fn lifecycle(node: &Node, runtime: CrunRuntime) -> Lifecycle {
    let private = |name: &str| {
        let path = node.dir.join(name);
        std::fs::create_dir_all(&path).expect("a directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("private");
        path
    };
    let config = Config {
        capacity: Capacity {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: 10 << 30,
            gpus: BTreeSet::new(),
        },
        allow_host_jobs: false,
        mounts: MountRoots::new(&[]).expect("no mount roots"),
        scratch: ScratchRoot::open(&private("life-scratch")).expect("scratch"),
        files: None,
        state: StateRoot::open(&private("life-state")).expect("state"),
        stop_grace: LONG,
        bundle: None,
        quota: None,
    };
    let (life, _) = Lifecycle::adopt(config, Box::new(runtime), Box::new(Pass), Box::new(Still))
        .expect("adopts");
    life
}

/// Proves the end of a job that ended while no agent ran reaches the store and the result:
/// the first agent's uploader put a chunk of each stream and recorded it, and went with the
/// agent before the end was read; the restarted agent adopts the run from its exit record, its
/// uploader is finishing at once, the result its lifecycle reports carries `logs`, and the store
/// ends with the two finals and no chunk (the recorded ones deleted after them). Catches an
/// adoption that makes no uploader for a run that ended, or one that never tells it the end.
#[test]
fn a_job_that_ended_while_the_agent_was_down_gets_its_final() {
    let node = Node::new("resume-ended");
    let live = Live::new();
    let mut first = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("j1-say", 60_000);
    spec.log_sink = Some(sink("j1-say"));
    let id = first.create_and_start(&spec).expect("started");
    first.wait_ended(&id);
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    live.flush(&wake);
    live.flush(&wake);
    let chunks = [Stream::Stdout, Stream::Stderr].map(|s| prefix("j1-say").chunk_key(s, 0));
    let objects = live.store.objects();
    assert!(
        chunks.iter().all(|key| objects.contains_key(key)),
        "{objects:?}"
    );
    let record = std::fs::read_to_string(node.lease_of(&id.0).join(RECORD)).expect("a record");
    let record: serde_json::Value = serde_json::from_str(&record).expect("json");
    assert_eq!(record["streams"]["stdout"]["chunks"], json!([0]));
    drop(first);
    assert_eq!(wake.step(), Settled::Ended);

    let again = live.restarted();
    let second = node.open().with_live_logs(again.logs.clone());
    assert_eq!(second.swept().adopted, ["j1-say"]);
    assert_eq!(again.made(), 1, "the adopted run has an uploader");
    let mut life = lifecycle(&node, second);
    let result = (0..100)
        .find_map(|_| {
            life.step();
            life.held_jobs().first().and_then(|job| job.result.clone())
        })
        .expect("a result");
    let report = LogReport {
        stdout_bytes: 10,
        stderr_bytes: 10,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(result.logs, Some(report));
    run_out(&again.wake(0));
    let objects = live.store.objects();
    let finals = |stream| prefix("j1-say").final_key(stream);
    assert_eq!(objects[&finals(Stream::Stdout)], b"to-stdout\n");
    assert_eq!(objects[&finals(Stream::Stderr)], b"to-stderr\n");
    assert_eq!(objects.len(), 2, "the chunks were deleted: {objects:?}");
}

/// Proves a run that still runs goes on uploading after a restart: what the job wrote before
/// it is a chunk at 0, what it wrote while no agent ran is the next chunk, put by the uploader
/// the restarted runtime made from the record; the removal of the lease then goes through
/// while that uploader still holds the files, and the final is the whole stream. Catches a
/// restarted runtime that makes no uploader from a record, and a record written into a lease
/// directory that is being removed.
#[test]
fn a_running_job_adopted_with_its_record_goes_on_uploading() {
    let node = Node::new("resume-running");
    let live = Live::new();
    let mut first = node.open().with_live_logs(live.logs.clone());
    let mut spec = node.job("r1-hold", 600_000);
    spec.log_sink = Some(sink("r1-hold"));
    let id = held(&node, &mut first, &spec);
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    let lease = node.lease_of(&id.0);
    assert!(lease.join(RECORD).is_file(), "written as the upload began");
    let before = b"written while the agent ran\n";
    append(&lease.join("stdout"), before);
    live.flush(&wake);
    live.flush(&wake);
    drop(first);
    assert_eq!(wake.step(), Settled::Ended);
    let during = b"and while no agent ran\n";
    append(&lease.join("stdout"), during);

    let again = live.restarted();
    let mut second = node.open().with_live_logs(again.logs.clone());
    assert_eq!(second.swept().adopted, ["r1-hold"]);
    assert_eq!(second.poll(&id), Ok(Status::Running));
    assert_eq!(again.made(), 1);
    let wake = again.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    again.flush(&wake);
    let objects = live.store.objects();
    let key = |offset: usize| prefix("r1-hold").chunk_key(Stream::Stdout, offset as u64);
    assert_eq!(objects[&key(0)], before);
    assert_eq!(objects[&key(before.len())], during);

    finish(&mut second, &id);
    assert!(!lease.exists(), "the lease directory is gone");
    run_out(&wake);
    let whole = [before.as_slice(), during.as_slice()].concat();
    let objects = live.store.objects();
    assert_eq!(objects[&prefix("r1-hold").final_key(Stream::Stdout)], whole);
}

/// What a test puts in a lease directory in place of a good record.
type Plant = Box<dyn Fn(&Path)>;

/// Proves a record the restarted agent cannot trust stops nothing: a record that is not JSON,
/// one of version 2, one of another job's prefix and a link in place of the file are each
/// ignored with one warning, and the run is adopted and runs as it would without one, with no
/// uploader and nothing put. Catches an adoption that fails, or a record taken, on any of them.
#[test]
fn a_bad_record_does_not_stop_the_adoption() {
    let good = |run: &str| {
        json!({
            "version": 1,
            "sink": {"endpoint": "http://127.0.0.1:9000", "region": "test-1",
                "bucket": "logs", "prefix": format!("js/{run}/"), "path_style": true},
            "streams": {
                "stdout": {"next": 0, "cut": null, "chunks": [], "gap_done": false,
                    "final_done": false},
                "stderr": {"next": 0, "cut": null, "chunks": [], "gap_done": false,
                    "final_done": false},
            },
        })
    };
    let kinds: [(&str, Plant); 4] = [
        (
            "not-json",
            Box::new(|lease: &Path| std::fs::write(lease.join(RECORD), "{").expect("written")),
        ),
        (
            "version-2",
            Box::new(move |lease: &Path| {
                let mut value = good("r1-hold");
                value["version"] = json!(2);
                std::fs::write(lease.join(RECORD), value.to_string()).expect("written");
            }),
        ),
        (
            "other-job",
            Box::new(move |lease: &Path| {
                let value = good("r2-hold");
                std::fs::write(lease.join(RECORD), value.to_string()).expect("written");
            }),
        ),
        (
            "link",
            Box::new(move |lease: &Path| {
                let target = lease.join("elsewhere.json");
                std::fs::write(&target, good("r1-hold").to_string()).expect("written");
                std::os::unix::fs::symlink(&target, lease.join(RECORD)).expect("a link");
            }),
        ),
    ];
    for (kind, make) in kinds {
        let node = Node::new(&format!("resume-bad-{kind}"));
        let mut first = node.open();
        let id = held(&node, &mut first, &node.job("r1-hold", 600_000));
        make(&node.lease_of(&id.0));
        drop(first);

        let live = Live::new();
        let lines = Lines::default();
        let guard = lines.capture();
        let mut second = node.open().with_live_logs(live.logs.clone());
        drop(guard);
        assert_eq!(second.swept().adopted, ["r1-hold"], "{kind}");
        assert_eq!(second.poll(&id), Ok(Status::Running), "{kind}");
        assert_eq!(live.made(), 0, "{kind}: no uploader");
        let text = lines.text();
        assert_eq!(
            text.matches("the progress record is ignored").count(),
            1,
            "{kind}: {text}"
        );
        finish(&mut second, &id);
        assert!(live.store.operations().is_empty(), "{kind}");
    }
}

/// Proves that what has no record is as it was: a run whose start carried no sink, on a
/// runtime that uploads, writes no record and has no uploader, and the restarted runtime adopts
/// it with none. Catches a record or an uploader where no live logs were asked for.
#[test]
fn without_a_record_nothing_changes() {
    let node = Node::new("resume-none");
    let live = Live::new();
    let mut first = node.open().with_live_logs(live.logs.clone());
    let id = held(&node, &mut first, &node.job("r1-hold", 600_000));
    assert!(!node.lease_of(&id.0).join(RECORD).exists());
    drop(first);

    let again = live.restarted();
    let mut second = node.open().with_live_logs(again.logs.clone());
    assert_eq!(second.swept().adopted, ["r1-hold"]);
    assert_eq!(live.made() + again.made(), 0, "no uploader");
    second.stop(&id, LONG).expect("stopped");
    assert!(matches!(second.poll(&id), Ok(Status::Exited(_))));
    assert_eq!(second.logs_report(&id), None);
    second.remove(&id).expect("removed");
    assert!(live.store.operations().is_empty());
}
