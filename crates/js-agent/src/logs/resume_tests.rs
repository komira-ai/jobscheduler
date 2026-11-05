//! An agent restart against the in-memory store: a run's uploader started as a start starts
//! it, over a lease directory made for the test; the agent's restart is the drop of its
//! handle; the adoption is [`RunLogs::resume`] over the same directory, as the `crun` runtime
//! calls it. The helpers are those of `tests`; nothing sleeps.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{LogGrantState, LogReport, LogSink};
use js_logstore::fake::{Op, Status};
use js_logstore::{Clock as _, JobPrefix, Stream};
use serde_json::{Value, json};

use super::grant::epoch_ms;
use super::record::{RECORD_FILE, Streams, read};
use super::tests::{BASE, Host, Lines, MIB, bytes, endpoint, puts};
use super::{HostGrant, ManualWake, RunLogs, Settled, Sink};

/// The agent's user, as the runtime names it.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The lease directory of one job: its two capture files, which the test appends to as the
/// capture does, and the bytes it wrote.
struct Lease {
    job: String,
    dir: PathBuf,
    prefix: JobPrefix,
    written: [Vec<u8>; 2],
}

impl Lease {
    fn new(host: &Host, job: &str) -> Self {
        let dir = host.dir.join(format!("lease-{job}"));
        fs::create_dir(&dir).expect("the lease directory");
        File::create(dir.join("stdout")).expect("stdout");
        File::create(dir.join("stderr")).expect("stderr");
        Self {
            job: job.to_owned(),
            dir,
            prefix: JobPrefix::new(BASE, job).expect("a prefix"),
            written: [Vec::new(), Vec::new()],
        }
    }

    fn sink(&self) -> Sink {
        Sink::new(endpoint(), self.prefix.clone())
    }

    fn files(&self) -> (PathBuf, PathBuf) {
        (self.dir.join("stdout"), self.dir.join("stderr"))
    }

    fn write(&mut self, stream: Stream, data: &[u8]) {
        let index = stream as usize;
        let (stdout, stderr) = self.files();
        let paths = [stdout, stderr];
        let mut file = OpenOptions::new()
            .append(true)
            .open(&paths[index])
            .expect("open");
        file.write_all(data).expect("append");
        self.written[index].extend_from_slice(data);
    }

    fn record_path(&self) -> PathBuf {
        self.dir.join(RECORD_FILE)
    }

    fn record(&self) -> Value {
        let text = fs::read_to_string(self.record_path()).expect("the record");
        serde_json::from_str(&text).expect("json")
    }

    /// The run's uploader, started as the runtime starts it for a run whose start carried the
    /// sink; its first step is over.
    fn start(&self, host: &Host) -> Running {
        let (wake, logs) = host.logs_for_test();
        let mut run = RunLogs::asked(Some(&logs), Some(&self.sink()));
        let (stdout, stderr) = self.files();
        run.start(Some(&logs), (&stdout, &stderr), &self.dir);
        assert_eq!(wake.settled(), Settled::Asleep, "the first step");
        Running { run, wake }
    }

    /// The run as a restarted agent adopts it: ended with `ended` totals while no agent ran, or
    /// not. Returns what its first step did, when it got an uploader.
    fn resume(&self, host: &Host, ended: Option<(u64, u64)>) -> (Running, Option<Settled>) {
        let (wake, logs) = host.logs_for_test();
        let mut run = RunLogs::adopted(ended);
        let (stdout, stderr) = self.files();
        run.resume(&logs, (&stdout, &stderr), &self.dir, (&self.job, me()));
        let first = run.uploader.is_some().then(|| wake.settled());
        (Running { run, wake }, first)
    }

    /// Every object under the stream's prefix, which must be chunks that follow each other
    /// from 0 with no overlap and no hole; their bytes, joined.
    fn contiguous(&self, host: &Host, stream: Stream) -> Vec<u8> {
        let mut joined = Vec::new();
        let under = self.prefix.stream_prefix(stream);
        for (key, body) in host.store.objects() {
            if key.starts_with(&under) {
                let at = joined.len() as u64;
                assert_eq!(key, self.prefix.chunk_key(stream, at), "the chunk at {at}");
                joined.extend_from_slice(&body);
            }
        }
        joined
    }
}

/// One run's live logs and the wake of its uploader.
struct Running {
    run: RunLogs,
    wake: Arc<ManualWake>,
}

impl Running {
    fn step(&self) -> Settled {
        self.wake.step()
    }

    /// The agent stops: the handle is dropped with the agent, and the thread ends at its next
    /// step.
    fn restart(self) {
        let Self { run, wake } = self;
        drop(run);
        assert_eq!(wake.step(), Settled::Ended);
    }
}

/// The record of one stream that has stored up to `next`, with `chunks` put and no cut.
fn stored(next: usize, chunks: &[usize]) -> Value {
    json!({"next": next, "cut": null, "chunks": chunks, "gap_done": false, "final_done": false})
}

/// Catches a resume that starts anywhere but at the first byte not stored (a repeated or a
/// skipped byte): an uploader that has put 3 MiB of stdout, and nothing of the 100 bytes of
/// stderr, goes with the agent; the files grow to 5 MiB; the adopted uploader puts the rest,
/// and the store holds chunks that follow each other from 0 to 5 MiB with the files' bytes,
/// and stderr's 100 bytes from 0.
#[test]
fn a_restarted_agent_resumes_at_the_next_byte_and_the_stream_is_contiguous() {
    let host = Host::new("resume-next");
    let mut lease = Lease::new(&host, "next");
    let first = lease.start(&host);
    lease.write(Stream::Stdout, &bytes(3 * MIB, 1));
    lease.write(Stream::Stderr, &bytes(100, 2));
    assert_eq!(first.step(), Settled::Asleep);
    assert_eq!(
        lease.record()["streams"]["stdout"],
        stored(3 * MIB, &[0, MIB, 2 * MIB])
    );
    assert_eq!(lease.record()["streams"]["stderr"], stored(0, &[]));
    first.restart();

    lease.write(Stream::Stdout, &bytes(2 * MIB, 3));
    let (second, began) = lease.resume(&host, None);
    assert_eq!(began, Some(Settled::Asleep));
    host.advance(super::LOG_FLUSH);
    assert_eq!(second.step(), Settled::Asleep);
    assert_eq!(lease.contiguous(&host, Stream::Stdout), lease.written[0]);
    assert_eq!(lease.contiguous(&host, Stream::Stderr), lease.written[1]);
    let mib = MIB as u64;
    let chunks: Vec<(String, usize)> = (0..5)
        .map(|n| (lease.prefix.chunk_key(Stream::Stdout, n * mib), MIB))
        .chain([(lease.prefix.chunk_key(Stream::Stderr, 0), 100)])
        .collect();
    assert_eq!(puts(&host.ops()), chunks, "each chunk put once");
}

/// Catches a chunk cut again after a restart (an overlapping chunk of another length): the
/// record names the cut of 100 bytes at 0, whose put went through with its answer lost, or
/// failed; the files grow by a mebibyte meanwhile. The first put after the resume is the key
/// at 0 with exactly 100 bytes, and the store ends with one object there and chunks that follow
/// each other.
#[test]
fn a_cut_in_flight_at_the_restart_is_put_again_with_the_same_length() {
    for stored_before in [true, false] {
        let host = Host::new(&format!("resume-cut-{stored_before}"));
        let mut lease = Lease::new(&host, "cut");
        let first = lease.start(&host);
        lease.write(Stream::Stdout, &bytes(100, 1));
        assert_eq!(first.step(), Settled::Asleep);
        host.advance(super::LOG_FLUSH);
        match stored_before {
            true => host.store.lose_response_of_next_put(),
            false => host.store.fail_next(1, Status::SlowDown),
        }
        assert_eq!(first.step(), Settled::Asleep);
        let cut = json!({"offset": 0, "len": 100});
        assert_eq!(lease.record()["streams"]["stdout"]["cut"], cut);
        let key = lease.prefix.chunk_key(Stream::Stdout, 0);
        assert_eq!(host.store.objects().contains_key(&key), stored_before);
        first.restart();

        lease.write(Stream::Stdout, &bytes(MIB, 2));
        let before = host.ops().len();
        let (second, _) = lease.resume(&host, None);
        assert_eq!(second.step(), Settled::Asleep);
        let after = puts(&host.ops()[before..]);
        assert_eq!(after[0], (key.clone(), 100), "{stored_before}");
        assert_eq!(host.store.objects()[&key].len(), 100);
        assert_eq!(lease.contiguous(&host, Stream::Stdout), lease.written[0]);
    }
}

/// Catches a cut recorded only after its put (a restart during the put would cut again): the
/// record is there from the uploader's start; a first put that fails and its retry, which the
/// store holds, leave the record naming the cut and nothing stored; once the store lets the
/// put go, the record has `next` advanced, the chunk listed and no cut. The record names the
/// sink as the start gave it.
#[test]
fn the_record_is_written_before_the_put_and_after_it() {
    let host = Host::new("record-order");
    let mut lease = Lease::new(&host, "order");
    let first = lease.start(&host);
    let record = lease.record();
    assert_eq!(record["version"], 1);
    let sink = json!({
        "endpoint": "http://127.0.0.1:9000",
        "region": "test-1",
        "bucket": "logs",
        "prefix": "js/order/",
        "path_style": true,
    });
    assert_eq!(record["sink"], sink);
    assert_eq!(record["streams"]["stdout"], stored(0, &[]));
    let mode = fs::metadata(lease.record_path())
        .expect("the record")
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );

    lease.write(Stream::Stdout, &bytes(MIB, 1));
    host.store.fail_next(1, Status::SlowDown);
    assert_eq!(first.step(), Settled::Asleep);
    let cut = json!({"next": 0, "cut": {"offset": 0, "len": MIB}, "chunks": [],
        "gap_done": false, "final_done": false});
    assert_eq!(lease.record()["streams"]["stdout"], cut);
    host.advance(Duration::from_secs(2));
    host.store.block_puts();
    let wake = first.wake.clone();
    std::thread::scope(|scope| {
        let stepped = scope.spawn(move || wake.step());
        host.store.wait_for_held_puts(1);
        assert_eq!(
            lease.record()["streams"]["stdout"],
            cut,
            "on record before the put"
        );
        assert!(host.store.objects().is_empty());
        host.store.release_puts();
        assert_eq!(stepped.join().expect("stepped"), Settled::Asleep);
    });
    assert_eq!(lease.record()["streams"]["stdout"], stored(MIB, &[0]));
    assert!(!lease.dir.join(".logs-upload.json.tmp").exists());
}

/// Catches bytes written while no agent ran going without their own first-byte time: 10 bytes
/// are put, the agent goes for ten minutes while the job writes 20 more, and the chunk made of
/// those carries the time of the first poll after the resume.
#[test]
fn bytes_written_while_the_agent_was_down_are_uploaded() {
    let host = Host::new("resume-down");
    let mut lease = Lease::new(&host, "down");
    let first = lease.start(&host);
    lease.write(Stream::Stdout, &bytes(10, 1));
    first.step();
    host.advance(super::LOG_FLUSH);
    first.step();
    first.restart();
    host.advance(Duration::from_secs(300));
    lease.write(Stream::Stdout, &bytes(20, 2));
    host.advance(Duration::from_secs(300));
    let resumed_at = host.clock.now();
    let (second, _) = lease.resume(&host, None);
    host.advance(super::LOG_FLUSH);
    second.step();
    let key = lease.prefix.chunk_key(Stream::Stdout, 10);
    assert_eq!(host.first_byte_at(&key), epoch_ms(resumed_at));
    assert_eq!(lease.contiguous(&host, Stream::Stdout), lease.written[0]);
}

/// Catches a resumed uploader that puts without a credential, an agent that never says it
/// holds none, and one that needs a credential per job: two jobs are adopted by an agent that
/// holds no credential; the host reports one state, stale with no expiry, and nothing is put
/// however long they wait; one grant in the host's cell, and both jobs' streams go on.
#[test]
fn a_restarted_agent_asks_once_for_its_host_credential_and_the_resumed_uploaders_continue_with_it()
{
    let mut host = Host::new("resume-grant");
    let mut leases = [Lease::new(&host, "g1"), Lease::new(&host, "g2")];
    for lease in &mut leases {
        let first = lease.start(&host);
        lease.write(Stream::Stdout, &bytes(MIB, 1));
        first.step();
        first.restart();
    }
    let before = host.ops();
    assert_eq!(puts(&before).len(), 2);

    host.grant = Arc::new(HostGrant::new(host.clock.clone()));
    let stale = LogGrantState {
        stale: true,
        until_ms: None,
    };
    assert_eq!(host.grant.state(), stale);
    let mut resumed = Vec::new();
    for lease in &mut leases {
        lease.write(Stream::Stdout, &bytes(10, 2));
        lease.write(Stream::Stderr, &bytes(10, 3));
        resumed.push(lease.resume(&host, None).0);
    }
    assert_eq!(host.grant.state(), stale, "one state for the host");
    for _ in 0..3 {
        host.advance(Duration::from_secs(60));
        resumed
            .iter()
            .for_each(|run| assert_eq!(run.step(), Settled::Asleep));
    }
    assert_eq!(host.ops(), before, "nothing without a credential");

    let grant = host.renew();
    resumed
        .iter()
        .for_each(|run| assert_eq!(run.step(), Settled::Asleep));
    for lease in &leases {
        assert_eq!(lease.contiguous(&host, Stream::Stdout), lease.written[0]);
        assert_eq!(lease.contiguous(&host, Stream::Stderr), lease.written[1]);
    }
    let held = LogGrantState {
        stale: false,
        until_ms: Some(grant.expires_at_ms),
    };
    assert_eq!(host.grant.state(), held);
}

/// Catches a job that ended while no agent ran being left without its final, or with chunks
/// deleted before it: 1.5 MiB of stdout was half put when the agent went, and the job ended
/// with 2 MiB and 64 bytes of stderr. The adopted uploader is finishing at once: it puts the
/// rest, the two finals from the files, and then deletes every chunk, those the record named
/// among them; the report is the exit record's totals.
#[test]
fn an_adopted_run_that_ended_finishes_from_its_record() {
    let host = Host::new("resume-ended");
    let mut lease = Lease::new(&host, "ended");
    let first = lease.start(&host);
    lease.write(Stream::Stdout, &bytes(3 * MIB / 2, 1));
    first.step();
    first.restart();
    assert_eq!(lease.record()["streams"]["stdout"]["chunks"], json!([0]));
    lease.write(Stream::Stdout, &bytes(MIB / 2, 2));
    lease.write(Stream::Stderr, &bytes(64, 3));

    let totals = (2 * MIB as u64, 64);
    let (second, began) = lease.resume(&host, Some(totals));
    let report = LogReport {
        stdout_bytes: totals.0,
        stderr_bytes: totals.1,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(second.run.report(), Some(report));
    let mut state = began.expect("an uploader");
    for _ in 0..10 {
        if state == Settled::Ended {
            break;
        }
        state = second.step();
    }
    assert_eq!(state, Settled::Ended);
    let objects = host.store.objects();
    assert_eq!(objects.len(), 2, "only the finals: {:?}", objects.keys());
    assert_eq!(
        objects[&lease.prefix.final_key(Stream::Stdout)],
        lease.written[0]
    );
    assert_eq!(
        objects[&lease.prefix.final_key(Stream::Stderr)],
        lease.written[1]
    );
    let ops = host.ops();
    let finals = ops
        .iter()
        .position(|op| matches!(op, Op::Put { key, .. } if key.ends_with(".log")))
        .expect("a final");
    let first_delete = ops
        .iter()
        .position(|op| matches!(op, Op::Delete { .. }))
        .expect("a delete");
    assert!(finals < first_delete, "{ops:?}");
    assert!(ops.contains(&Op::Delete {
        key: lease.prefix.chunk_key(Stream::Stdout, 0)
    }));
    let done = json!({"next": 2 * MIB, "cut": null, "chunks": [], "gap_done": false,
        "final_done": true});
    assert_eq!(lease.record()["streams"]["stdout"], done);
}

/// Catches a record that lets a secret reach the disk: through a run with a host credential,
/// its chunks, final and deletes, no version of the record holds the key id, the secret key or
/// the session token.
#[test]
fn the_record_holds_no_secret() {
    let host = Host::new("record-secret");
    let grant = host.renew();
    let secrets = [
        Some(grant.access_key_id.clone()),
        Some(grant.secret_access_key.clone()),
        grant.session_token.clone(),
    ];
    assert!(grant.session_token.is_some(), "a token to look for");
    let mut lease = Lease::new(&host, "secret");
    let first = lease.start(&host);
    let mut seen = vec![fs::read(lease.record_path()).expect("the record")];
    lease.write(Stream::Stdout, &bytes(3 * MIB / 2, 1));
    lease.write(Stream::Stderr, &bytes(10, 2));
    first.step();
    seen.push(fs::read(lease.record_path()).expect("the record"));
    first
        .run
        .uploader
        .as_ref()
        .expect("an uploader")
        .finish((3 * MIB as u64 / 2, 10));
    assert_eq!(first.step(), Settled::Ended);
    seen.push(fs::read(lease.record_path()).expect("the record"));
    assert_eq!(
        lease.record()["streams"]["stderr"]["final_done"],
        json!(true)
    );
    for record in &seen {
        let text = String::from_utf8_lossy(record);
        for secret in secrets.iter().flatten() {
            assert!(!text.contains(secret.as_str()), "{text}");
        }
    }
}

/// What a test makes in place of a lease's record.
type Plant = (&'static str, fn(&Lease));

/// Writes `value` as the record of `lease`.
fn plant(lease: &Lease, value: &Value) {
    fs::write(lease.record_path(), value.to_string()).expect("a record");
}

/// A good record of `lease` as JSON, from a run that stored nothing yet.
fn good(lease: &Lease) -> Value {
    json!({
        "version": 1,
        "sink": {"endpoint": "http://127.0.0.1:9000", "region": "test-1", "bucket": "logs",
            "prefix": lease.prefix.as_str(), "path_style": true},
        "streams": {"stdout": stored(0, &[]), "stderr": stored(0, &[])},
    })
}

/// Catches an adoption that fails, or follows a link, on a record it cannot trust: a record
/// that is not JSON, one of version 2, one of another job's prefix, a link in place of the
/// file, a file of another user, a directory, a record with a field missing, and one whose sink
/// the store client refuses, each give no uploader, one warning and a run that is adopted as
/// it would be without one (no report, nothing put); the link's target is not read. No record
/// gives no uploader and no warning. A field the reader does not know is ignored.
#[test]
fn a_bad_record_is_ignored_and_adoption_goes_on() {
    let plants: [Plant; 8] = [
        ("not JSON", |lease| {
            fs::write(lease.record_path(), b"{\"version\":1,").expect("written")
        }),
        ("version 2", |lease| {
            let mut value = good(lease);
            value["version"] = json!(2);
            value["streams"] = json!("another form");
            plant(lease, &value);
        }),
        ("another job's", |lease| {
            let mut value = good(lease);
            value["sink"]["prefix"] = json!("js/other/");
            plant(lease, &value);
        }),
        ("a link", |lease| {
            let target = lease.dir.join("elsewhere.json");
            plant(lease, &good(lease));
            fs::rename(lease.record_path(), &target).expect("moved");
            symlink(&target, lease.record_path()).expect("a link");
        }),
        ("another user's", |lease| plant(lease, &good(lease))),
        ("a directory", |lease| {
            fs::create_dir(lease.record_path()).expect("a directory")
        }),
        ("a field missing", |lease| {
            let mut value = good(lease);
            value["streams"]["stderr"] = json!({"next": 0});
            plant(lease, &value);
        }),
        ("a sink it cannot use", |lease| {
            let mut value = good(lease);
            value["sink"]["region"] = json!("Not A Region");
            plant(lease, &value);
        }),
    ];
    for (n, (what, make)) in plants.into_iter().enumerate() {
        let host = Host::new(&format!("bad-record-{n}"));
        let lease = Lease::new(&host, "bad");
        make(&lease);
        let lines = Lines::default();
        let guard = lines.capture();
        let (wake, logs) = host.logs_for_test();
        let mut run = RunLogs::adopted(Some((5, 5)));
        let (stdout, stderr) = lease.files();
        let owner = match what {
            "another user's" => me() + 1,
            _ => me(),
        };
        run.resume(&logs, (&stdout, &stderr), &lease.dir, ("bad", owner));
        drop(guard);
        assert!(run.uploader.is_none(), "{what}");
        assert_eq!(run.report(), None, "{what}");
        assert_eq!(wake.notified(), 0, "{what}");
        let text = lines.text();
        assert_eq!(
            text.matches("the progress record is ignored").count(),
            1,
            "{what}: {text}"
        );
        assert!(host.ops().is_empty(), "{what}");
    }

    // A link is refused before it is read: its target is never opened.
    let host = Host::new("bad-record-link-target");
    let lease = Lease::new(&host, "bad");
    symlink(lease.dir.join("missing"), lease.record_path()).expect("a dangling link");
    let why = read(&lease.dir, "bad", me()).expect_err("a link");
    assert!(why.starts_with("it cannot be opened"), "{why}");

    // No record: no uploader and nothing said.
    let lines = Lines::default();
    let guard = lines.capture();
    let empty = Lease::new(&host, "none");
    let (_, logs) = host.logs_for_test();
    let mut run = RunLogs::adopted(None);
    let (stdout, stderr) = empty.files();
    run.resume(&logs, (&stdout, &stderr), &empty.dir, ("none", me()));
    drop(guard);
    assert!(run.uploader.is_none());
    assert_eq!(lines.text(), "");

    // A field it does not know is ignored.
    let known = Lease::new(&host, "known");
    let mut value = good(&known);
    value["later"] = json!({"any": "thing"});
    value["streams"]["stdout"]["later"] = json!(1);
    plant(&known, &value);
    let (sink, streams) = read(&known.dir, "known", me())
        .expect("read")
        .expect("a record");
    assert_eq!(sink, known.sink());
    assert_eq!(streams, Streams::default());
}

/// Catches a record that cannot be written stopping the upload, or one left behind that lags
/// behind the store: with a directory where the record goes, the uploader logs one warning,
/// writes no record from then on, and its chunks go out all the same.
#[test]
fn a_record_that_cannot_be_written_is_given_up_and_the_upload_goes_on() {
    let host = Host::new("record-fails");
    let mut lease = Lease::new(&host, "fails");
    fs::create_dir(lease.record_path()).expect("a directory in its place");
    let lines = Lines::default();
    let guard = lines.capture();
    let first = lease.start(&host);
    lease.write(Stream::Stdout, &bytes(2 * MIB, 1));
    assert_eq!(first.step(), Settled::Asleep);
    drop(guard);
    let text = lines.text();
    assert_eq!(
        text.matches("the progress record could not be written")
            .count(),
        1,
        "{text}"
    );
    assert!(lease.record_path().is_dir(), "nothing was written over it");
    assert_eq!(lease.contiguous(&host, Stream::Stdout), lease.written[0]);
}

/// Catches a record written into a lease directory the runtime is removing, and one left
/// behind, frozen, by a removal that failed: once the handle was told to leave, the record
/// written is gone and no record is written again, while the upload goes on to its final.
#[test]
fn a_run_that_left_its_lease_writes_no_record() {
    let host = Host::new("record-left");
    let mut lease = Lease::new(&host, "left");
    let first = lease.start(&host);
    assert!(lease.record_path().is_file());
    first.run.leave();
    assert!(!lease.record_path().exists(), "removed by the leave");
    lease.write(Stream::Stdout, &bytes(10, 1));
    first
        .run
        .uploader
        .as_ref()
        .expect("an uploader")
        .finish((10, 0));
    assert_eq!(first.step(), Settled::Ended);
    assert!(!lease.record_path().exists());
    let objects = host.store.objects();
    assert_eq!(
        objects[&lease.prefix.final_key(Stream::Stdout)],
        lease.written[0]
    );
}

/// Catches a sink that the record cannot give back as the start gave it: a path-style and a
/// virtual-host-style sink (whose host begins with the bucket's name) are recorded with the
/// URL, region, bucket, prefix and style they were made from.
#[test]
fn a_sink_is_recorded_as_its_start_gave_it() {
    for (endpoint, path_style) in [
        ("http://127.0.0.1:9000", true),
        ("https://logs.example", true),
        ("https://logs.example", false),
    ] {
        let wire = LogSink {
            endpoint: endpoint.to_owned(),
            region: "test-1".to_owned(),
            bucket: "logs".to_owned(),
            prefix: "js/j1/".to_owned(),
            path_style,
        };
        let sink = Sink::of_wire(&wire).expect("a sink");
        assert_eq!(sink.wire(), wire, "{endpoint} {path_style}");
        assert_eq!(Sink::of_wire(&sink.wire()), Ok(sink));
    }
}

/// Catches a resumed uploader that tails whatever the job user put in place of a capture file
/// while no agent ran: with a good record, a link to another file in place of stdout, and a
/// directory in place of stderr, each gives no uploader and one warning, and the link's target
/// is never read into the store.
#[test]
fn a_capture_file_replaced_while_no_agent_ran_is_not_opened() {
    for (n, stream) in ["stdout", "stderr"].into_iter().enumerate() {
        let host = Host::new(&format!("replaced-capture-{n}"));
        let lease = Lease::new(&host, "replaced");
        plant(&lease, &good(&lease));
        let path = lease.dir.join(stream);
        fs::remove_file(&path).expect("the capture file");
        match stream {
            "stdout" => {
                let secret = host.dir.join("host-secret");
                fs::write(&secret, b"not the job's").expect("a file");
                symlink(&secret, &path).expect("a link");
            }
            _ => fs::create_dir(&path).expect("a directory"),
        }
        let lines = Lines::default();
        let guard = lines.capture();
        let (wake, logs) = host.logs_for_test();
        let mut run = RunLogs::adopted(None);
        let (stdout, stderr) = lease.files();
        run.resume(&logs, (&stdout, &stderr), &lease.dir, ("replaced", me()));
        drop(guard);
        assert!(run.uploader.is_none(), "{stream}");
        assert_eq!(wake.notified(), 0, "{stream}");
        let text = lines.text();
        assert_eq!(text.matches("did not start").count(), 1, "{stream}: {text}");
        assert!(host.ops().is_empty(), "{stream}");
    }
}
