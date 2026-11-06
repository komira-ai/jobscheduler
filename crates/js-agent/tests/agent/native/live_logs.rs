//! Live logs of the native runtime over the fake helper: a run whose start carries a sink has
//! its two log files uploaded to an in-memory store by an uploader the test steps (its wake is
//! manual and its clock a manual one), output past the file cap goes up as `crun`'s capture
//! would send it, a restarted agent finishes an upload from its progress record, and a run
//! without a sink uploads nothing. The test writes the job's output into the files the
//! runtime gave the job, as the job would.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status, label};
use js_agent::logs::{HostGrant, LOG_FLUSH, Limits, LiveLogs, ManualWake, Settled, Sink, Wake};
use js_agent::native::NativeRuntime;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;
use js_agent_wire::{LogGrant, LogReport, LogTail};
use js_logstore::fake::{Action, FakeStore, Op, Policy};
use js_logstore::{Endpoint, JobPrefix, ManualClock, Stream};
use js_mac_session::helper::RESET_ALREADY;
use serde_json::{Value, json};

use super::agent::{hello_on, native_flags};
use super::{Rig, TERM, config, native};
use crate::support::{
    CountingSleeper, Jobs, Lowest, Scratch, Script, ScriptConnector, code, flags, mac_host, reply,
    run_on, welcome,
};

/// The progress record an uploader keeps in the run's scratch directory.
const RECORD: &str = "logs-upload.json";

/// The store's address: only a name for the in-memory store, which opens no socket.
fn endpoint() -> Endpoint {
    Endpoint::parse("http://127.0.0.1:9000", "test-1", "logs", true).expect("an endpoint")
}

fn prefix(run_id: &str) -> JobPrefix {
    JobPrefix::new("js", run_id).expect("a prefix")
}

fn sink(run_id: &str) -> Sink {
    Sink::new(endpoint(), prefix(run_id))
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// A store, the host's credential, and the live-log settings of a runtime that uploads to it
/// with the file cap `cap`; the wakes of its uploaders are kept in the order they were made.
struct Live {
    store: FakeStore,
    clock: Arc<ManualClock>,
    logs: LiveLogs,
    wakes: Arc<Mutex<Vec<Arc<ManualWake>>>>,
}

impl Live {
    fn new(cap: u64) -> Self {
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
            file_cap: cap,
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

    /// The objects of the store.
    fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.store.objects()
    }

    /// The keys of every put, in the order they were made.
    fn puts(&self) -> Vec<String> {
        let ops = self.store.operations().into_iter();
        ops.filter_map(|op| match op {
            Op::Put { key, .. } => Some(key),
            _ => None,
        })
        .collect()
    }
}

/// Steps the uploader of `wake` until it has ended.
fn run_out(wake: &ManualWake) {
    while wake.step() != Settled::Ended {}
}

/// Appends `bytes` to the log file `stream` of the run whose scratch directory is `dir`, as
/// the job writes to it.
fn print(dir: &Path, stream: &str, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.join(stream))
        .unwrap();
    file.write_all(bytes).unwrap();
}

/// Lets the uploader of `wake` see what was printed, then flushes it: the clock moves past
/// `LOG_FLUSH`, and the next step puts what waited.
fn flush(live: &Live, wake: &ManualWake) {
    assert_eq!(wake.step(), Settled::Asleep);
    live.clock.advance(LOG_FLUSH);
    assert_eq!(wake.step(), Settled::Asleep);
}

/// What the chunks of `stream` of `run_id` hold, joined in offset order.
fn chunks(objects: &BTreeMap<String, Vec<u8>>, run_id: &str, stream: Stream) -> Vec<u8> {
    let under = format!("{}{}/", prefix(run_id).as_str(), stream.name());
    objects
        .iter()
        .filter(|(key, _)| key.starts_with(&under) && !key.ends_with(".gap"))
        .flat_map(|(_, body)| body.clone())
        .collect()
}

/// A rig whose runtime uploads with `live`, and a run of it whose start carries a sink, started.
fn started(name: &str, live: &Live, run_id: &str) -> (Rig, RunHandle, std::path::PathBuf) {
    let mut rig = Rig::new(name).living(live.logs.clone());
    let mut spec = rig.spec(run_id, native("/bin/echo", &[]));
    spec.log_sink = Some(sink(run_id));
    let id = rig.rt.create_and_start(&spec).unwrap();
    (rig, id, spec.scratch)
}

/// Proves the live upload of a native run that writes its two streams in turns: while the
/// job runs, the store holds every byte of each stream in order, as chunks at their offsets;
/// the end is told at the poll that reads the exit, before the run's end is answered, with
/// the files' lengths as the totals; the uploader then writes each stream's final and deletes
/// the chunks; the tail is the files' as before; and the removal takes the progress record
/// away. Catches a native run with a sink that uploads nothing, out of order or under other
/// keys, a report missing from the result, and a record left for the cleanup to meet.
#[test]
fn a_native_run_with_a_sink_uploads_both_streams_while_it_runs() {
    let live = Live::new(64 * 1024 * 1024);
    let (mut rig, id, dir) = started("logs-live", &live, "j1");
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep, "the uploader's first step");
    assert!(
        dir.join(RECORD).is_file(),
        "the record is kept beside the files"
    );
    print(&dir, "stdout", b"out-1\n");
    print(&dir, "stderr", b"err-1\n");
    flush(&live, &wake);
    print(&dir, "stderr", b"err-2\n");
    print(&dir, "stdout", b"out-2\n");
    flush(&live, &wake);

    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    assert_eq!(rig.rt.logs_report(&id), None, "no report before the end");
    let objects = live.objects();
    let p = prefix("j1");
    assert_eq!(
        objects.keys().cloned().collect::<Vec<_>>(),
        [
            p.chunk_key(Stream::Stderr, 0),
            p.chunk_key(Stream::Stderr, 6),
            p.chunk_key(Stream::Stdout, 0),
            p.chunk_key(Stream::Stdout, 6),
        ]
    );
    assert_eq!(chunks(&objects, "j1", Stream::Stdout), b"out-1\nout-2\n");
    assert_eq!(chunks(&objects, "j1", Stream::Stderr), b"err-1\nerr-2\n");

    rig.fake.exit("7.0", 0);
    assert_eq!(rig.settle(&id), exited(0));
    let report = LogReport {
        stdout_bytes: 12,
        stderr_bytes: 12,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(rig.rt.logs_report(&id), Some(report));
    assert!(wake.notified() > 0, "the end woke the uploader");
    run_out(&wake);
    let objects = live.objects();
    assert_eq!(
        objects,
        BTreeMap::from([
            (p.final_key(Stream::Stderr), b"err-1\nerr-2\n".to_vec()),
            (p.final_key(Stream::Stdout), b"out-1\nout-2\n".to_vec()),
        ])
    );
    let tail = LogTail {
        stdout: "out-1\nout-2\n".to_owned(),
        stderr: "err-1\nerr-2\n".to_owned(),
        truncated: false,
    };
    assert_eq!(rig.rt.log_tail(&id).unwrap(), Some(tail));
    rig.rt.remove(&id).unwrap();
    assert!(
        !dir.join(RECORD).exists(),
        "the record went before the directory"
    );
}

/// Proves that output past the file cap goes up as `crun`'s capture sends it for the same
/// output (it keeps the first `file_cap` bytes in its file and drops the rest): while the job
/// runs only the first `cap` bytes are put; at the end the stream gets a gap marker at the cap
/// for the rest and no final, the report counts the rest as dropped, and the other stream,
/// under the cap, gets its final. The tail is the last bytes the job wrote, cut to the wire's
/// limit and marked truncated, the text a capture's ring of the last bytes gives. Catches an
/// upload past the cap, a final over a gap, a report that drops nothing, and a tail taken from
/// the start of the stream.
#[test]
fn output_past_the_cap_is_uploaded_as_the_crun_capture_uploads_it() {
    const CAP: u64 = 100;
    let live = Live::new(CAP);
    let (mut rig, id, dir) = started("logs-cap", &live, "j2");
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    let out: Vec<u8> = (0..20_000u32)
        .map(|n| b"0123456789abcdef"[(n % 16) as usize])
        .collect();
    print(&dir, "stdout", &out);
    print(&dir, "stderr", b"e\n");
    flush(&live, &wake);
    let p = prefix("j2");
    let objects = live.objects();
    assert_eq!(objects[&p.chunk_key(Stream::Stdout, 0)], &out[..100]);
    assert_eq!(
        objects.len(),
        2,
        "nothing past the cap: {:?}",
        objects.keys()
    );

    rig.fake.exit("7.0", 0);
    assert_eq!(rig.settle(&id), exited(0));
    let report = LogReport {
        stdout_bytes: 20_000,
        stderr_bytes: 2,
        stdout_dropped: 20_000 - CAP,
        stderr_dropped: 0,
    };
    assert_eq!(rig.rt.logs_report(&id), Some(report));
    run_out(&wake);
    let gap = format!("{}.gap", p.chunk_key(Stream::Stdout, CAP));
    assert_eq!(
        live.objects(),
        BTreeMap::from([
            (p.chunk_key(Stream::Stdout, 0), out[..100].to_vec()),
            (gap, br#"{"length":19900}"#.to_vec()),
            (p.final_key(Stream::Stderr), b"e\n".to_vec()),
        ])
    );
    let end = &out[out.len() - MAX_LOG_STREAM_LEN..];
    let tail = LogTail {
        stdout: String::from_utf8(end.to_vec()).unwrap(),
        stderr: "e\n".to_owned(),
        truncated: true,
    };
    assert_eq!(rig.rt.log_tail(&id).unwrap(), Some(tail));
    rig.rt.remove(&id).unwrap();
}

/// Proves that an agent restart loses and repeats nothing of a native run's upload: the first
/// runtime puts a chunk, sees more output and is dropped with the job running (its uploader
/// stops, its record stays). The job writes on while no agent runs. The next runtime over the
/// same helper and journal adopts the run, and its upload goes on from the record: the bytes
/// seen but not put and those written meanwhile go out as the next chunk, and the exit ends it
/// with the finals. Each chunk offset was put once and no gap marker was put. Catches an
/// adoption that re-sends from the start, skips what was written while no agent ran, or
/// leaves an adopted run's upload undone.
#[test]
fn an_adopted_run_goes_on_from_the_record_without_a_gap_or_a_repeat() {
    let live = Live::new(64 * 1024 * 1024);
    let mut rig = Rig::new("logs-restart").living(live.logs.clone());
    let mut spec = rig.spec("j3", native("/bin/echo", &[]));
    spec.log_sink = Some(sink("j3"));
    spec.labels = BTreeMap::from([(label::RUN.to_owned(), "j3".to_owned())]);
    rig.rt.create_and_start(&spec).unwrap();
    let dir = spec.scratch;
    let first = live.wake(0);
    assert_eq!(first.settled(), Settled::Asleep);
    print(&dir, "stdout", b"0123456789");
    flush(&live, &first);
    print(&dir, "stdout", b"abcde");
    print(&dir, "stderr", b"oops\n");
    assert_eq!(first.step(), Settled::Asleep, "seen and not yet flushed");

    // The agent goes: its uploader stops at its next step and leaves the record.
    let Rig {
        fake,
        rt,
        dir: _kept,
    } = rig;
    drop(rt);
    run_out(&first);
    assert!(dir.join(RECORD).is_file());
    // The job writes on while no agent runs.
    print(&dir, "stdout", b"FGH");

    fake.refuse("boot-reset", RESET_ALREADY);
    let mut again = NativeRuntime::open(fake.client(), config(&fake), TERM + 1)
        .unwrap()
        .with_live_logs(live.logs.clone());
    assert!(again.reclaim().is_empty(), "the run is adopted, not ended");
    assert_eq!(again.list_labelled().unwrap().len(), 1);
    assert_eq!(live.made(), 2, "the adopted run has an uploader again");
    let resumed = live.wake(1);
    assert_eq!(resumed.settled(), Settled::Asleep);
    flush(&live, &resumed);
    let id = RunHandle("7.0".to_owned());
    fake.exit("7.0", 0);
    for _ in 0..super::POLLS {
        if again.poll(&id).unwrap() != Status::Running {
            break;
        }
        std::thread::yield_now();
    }
    let report = LogReport {
        stdout_bytes: 18,
        stderr_bytes: 5,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(again.logs_report(&id), Some(report));
    run_out(&resumed);
    again.remove(&id).unwrap();

    let p = prefix("j3");
    assert_eq!(
        live.puts(),
        [
            p.chunk_key(Stream::Stdout, 0),
            p.chunk_key(Stream::Stdout, 10),
            p.chunk_key(Stream::Stderr, 0),
            p.final_key(Stream::Stdout),
            p.final_key(Stream::Stderr),
        ]
    );
    assert_eq!(
        live.objects(),
        BTreeMap::from([
            (p.final_key(Stream::Stderr), b"oops\n".to_vec()),
            (p.final_key(Stream::Stdout), b"0123456789abcdeFGH".to_vec()),
        ])
    );
}

/// Proves that a run without a sink, on a runtime that uploads, is what it was: no uploader
/// (no wake, no request, no record), no `logs` in the report, and the tail of its files.
/// Catches an upload or a report where no live logs were asked for.
#[test]
fn a_native_run_without_a_sink_uploads_nothing() {
    let live = Live::new(64 * 1024 * 1024);
    let mut rig = Rig::new("logs-none").living(live.logs.clone());
    rig.fake.print(b"plain\n", b"");
    let id = rig.start("j4", "/bin/echo", &[]);
    rig.fake.exit("7.0", 0);
    assert_eq!(rig.settle(&id), exited(0));
    assert_eq!(rig.rt.logs_report(&id), None);
    let tail = LogTail {
        stdout: "plain\n".to_owned(),
        stderr: String::new(),
        truncated: false,
    };
    assert_eq!(rig.rt.log_tail(&id).unwrap(), Some(tail));
    assert_eq!(live.made(), 0, "no uploader");
    assert!(live.store.operations().is_empty(), "no request");
    assert!(!rig.dir.path("scratch").join("j4").join(RECORD).exists());
    rig.rt.remove(&id).unwrap();
}

/// Proves that a Mac agent says it uploads live logs and holds the host's credential cell: its
/// hello reports `log-upload`, and its first heartbeat reports the credential state (`stale`,
/// none held yet), so the server mints it one. (A sink rides only in a job start, which the
/// server sends a host that reports `per-job`; a native agent does not yet, so its runs carry
/// none until it does.) Catches a native agent that hides that it can upload, and one that
/// reports the capability without a cell for the credential.
#[test]
fn a_mac_agent_reports_log_upload_and_asks_for_the_credential() {
    let dir = Scratch::new("native-logs-hello");
    let fake = super::fake_helper::FakeHelper::start(&dir);
    let extra = native_flags(&dir, &fake);
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(hello["capabilities"]["log-upload"], "true");

    let again = Scratch::new("native-logs-beat");
    let other = super::fake_helper::FakeHelper::start(&again);
    let extra = native_flags(&again, &other);
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let beat = json!({"commands": [], "result_acks": [], "taint_ack": false});
    let (script, sent) = Script::new(vec![welcome(1), reply(200, beat)]);
    let mut sleeper = CountingSleeper::stopping_at(2);
    let machine = Arc::new(crate::jobs::fake::FakeMachine::new());
    let jobs: Jobs<'_> = (machine, None);
    let connector = ScriptConnector::of(script);
    let ran = run_on(
        &flags(&refs),
        &mac_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let bodies: Vec<Value> = sent.borrow().iter().map(|sent| sent.1.clone()).collect();
    let beats = &bodies[1..];
    assert!(!beats.is_empty(), "a hello and heartbeats: {bodies:?}");
    for beat in beats {
        assert_eq!(beat["log_grant_state"], json!({"stale": true}), "{beat}");
    }
}

/// Proves the boundary of the cap: a stream of exactly `cap` bytes is whole (one chunk, then
/// its final, nothing dropped) and one of a byte more is cut (the chunk of `cap` bytes stays,
/// and a gap marker of one byte follows it, with no final), as `crun`'s capture keeps a stream
/// that fits its file and cuts one that does not. Catches a cap off by one either way.
#[test]
fn a_stream_of_exactly_the_cap_is_whole_and_one_byte_more_is_cut() {
    const CAP: u64 = 10;
    let live = Live::new(CAP);
    let (mut rig, id, dir) = started("logs-edge", &live, "j5");
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    print(&dir, "stdout", b"0123456789");
    print(&dir, "stderr", b"0123456789X");
    rig.fake.exit("7.0", 0);
    assert_eq!(rig.settle(&id), exited(0));
    let report = LogReport {
        stdout_bytes: 10,
        stderr_bytes: 11,
        stdout_dropped: 0,
        stderr_dropped: 1,
    };
    assert_eq!(rig.rt.logs_report(&id), Some(report));
    run_out(&wake);
    let p = prefix("j5");
    let gap = format!("{}.gap", p.chunk_key(Stream::Stderr, CAP));
    assert_eq!(
        live.objects(),
        BTreeMap::from([
            (p.chunk_key(Stream::Stderr, 0), b"0123456789".to_vec()),
            (gap, br#"{"length":1}"#.to_vec()),
            (p.final_key(Stream::Stdout), b"0123456789".to_vec()),
        ])
    );
    rig.rt.remove(&id).unwrap();
}

/// Proves the end of a run whose exit was never read: removed while it runs (the lifecycle's
/// stop and removal, or a helper that lost track of it), it is killed, then told its end with
/// what its files hold, so its finals hold every byte and its record is gone. Catches an
/// upload a removal cuts short with neither final nor gap, and totals taken before the kill.
#[test]
fn a_run_removed_before_its_exit_was_read_is_finished_after_the_kill() {
    let live = Live::new(64 * 1024 * 1024);
    let (mut rig, id, dir) = started("logs-removed", &live, "j6");
    let wake = live.wake(0);
    assert_eq!(wake.settled(), Settled::Asleep);
    print(&dir, "stdout", b"half-");
    flush(&live, &wake);
    print(&dir, "stdout", b"done\n");
    rig.rt.stop(&id, Duration::ZERO).unwrap();
    rig.rt.remove(&id).unwrap();
    assert!(
        !dir.join(RECORD).exists(),
        "the record went before the directory"
    );
    run_out(&wake);
    let p = prefix("j6");
    assert_eq!(
        live.objects(),
        BTreeMap::from([(p.final_key(Stream::Stdout), b"half-done\n".to_vec())])
    );
}
