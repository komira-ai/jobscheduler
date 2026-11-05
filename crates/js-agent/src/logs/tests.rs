//! The uploader against the in-memory store: temporary files the test appends to as the
//! capture would, a manual clock and a manual wake. Nothing sleeps: each step is one call.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::{LogGrant, LogReport};
use js_logstore::fake::{Action as Can, FakeStore, Op, Policy, Status};
use js_logstore::{Clock, Credential, Endpoint, JobPrefix, ManualClock, Store, Stream};
use js_testkit::dir::TestDir;

use super::grant::epoch_ms;
use super::{
    CutLimits, GRANT_STALE_MARGIN, HostGrant, Limits, LiveLogs, ManualWake, RunLogs, Settled, Sink,
    Uploader, Wake, cut,
};

pub(super) const MIB: usize = 1024 * 1024;

/// The prefix of every job of these tests.
pub(super) const BASE: &str = "js";

pub(super) fn start_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

pub(super) fn endpoint() -> Endpoint {
    Endpoint::parse("http://127.0.0.1:9000", "test-1", "logs", true).expect("an endpoint")
}

/// The grant a heartbeat reply carries for `credential`.
pub(super) fn grant_of(credential: &Credential) -> LogGrant {
    LogGrant {
        access_key_id: credential.access_key_id.clone(),
        secret_access_key: credential.secret_access_key.clone(),
        session_token: credential.session_token.clone(),
        expires_at_ms: epoch_ms(credential.expires_at),
    }
}

/// `len` bytes of a pattern that holds every byte value, NULs and non-UTF-8 among them.
pub(super) fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u64).wrapping_mul(31).wrapping_add(u64::from(seed)) as u8)
        .collect()
}

/// A store, a clock and a host credential cell, shared by the uploaders of a test.
pub(super) struct Host {
    pub(super) dir: TestDir,
    pub(super) clock: Arc<ManualClock>,
    pub(super) store: FakeStore,
    pub(super) grant: Arc<HostGrant>,
    pub(super) limits: Limits,
}

impl Host {
    /// A host holding a credential that may put and delete under the prefix.
    pub(super) fn new(name: &str) -> Self {
        let host = Self::bare(name);
        host.renew();
        host
    }

    /// A host holding no credential.
    pub(super) fn bare(name: &str) -> Self {
        let clock = Arc::new(ManualClock::new(start_time()));
        let store = FakeStore::new(endpoint(), clock.clone());
        Self {
            dir: TestDir::under(std::env::temp_dir(), name),
            grant: Arc::new(HostGrant::new(clock.clone())),
            clock,
            store,
            limits: Limits::default(),
        }
    }

    /// A new credential from the store, put in the cell as a reply's grant would be.
    pub(super) fn renew(&self) -> LogGrant {
        self.renew_with(vec![Can::Put, Can::Delete])
    }

    /// A new credential that may do `actions`, put in the cell.
    pub(super) fn renew_with(&self, actions: Vec<Can>) -> LogGrant {
        let credential = self.store.issue(Policy {
            actions,
            prefix: format!("{BASE}/"),
        });
        let grant = grant_of(&credential);
        self.grant.replace(&grant);
        grant
    }

    pub(super) fn logs(&self, wake: Arc<ManualWake>) -> LiveLogs {
        LiveLogs {
            file_cap: 1 << 20,
            grant: self.grant.clone(),
            transport: Arc::new(self.store.clone()),
            clock: self.clock.clone(),
            wakes: Arc::new(move || wake.clone() as Arc<dyn Wake>),
            limits: self.limits,
        }
    }

    /// Starts the uploader of `job`, over two new empty files.
    pub(super) fn job(&self, job: &str) -> Job {
        self.job_with(job, self.logs_for_test())
    }

    pub(super) fn logs_for_test(&self) -> (Arc<ManualWake>, LiveLogs) {
        let wake = ManualWake::new();
        (wake.clone(), self.logs(wake))
    }

    pub(super) fn job_with(&self, job: &str, (wake, logs): (Arc<ManualWake>, LiveLogs)) -> Job {
        let stdout = self.dir.join(format!("{job}-stdout"));
        let stderr = self.dir.join(format!("{job}-stderr"));
        File::create(&stdout).expect("stdout");
        File::create(&stderr).expect("stderr");
        let prefix = JobPrefix::new(BASE, job).expect("a prefix");
        let sink = Sink::new(endpoint(), prefix.clone());
        let uploader = Uploader::start(&sink, (&stdout, &stderr), &logs).expect("started");
        assert_eq!(wake.settled(), Settled::Asleep, "the first step");
        Job {
            prefix,
            files: [stdout, stderr],
            written: [Vec::new(), Vec::new()],
            wake,
            uploader,
        }
    }

    pub(super) fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The keys put and deleted, in order, as `P` `put|delete` `key`.
    pub(super) fn ops(&self) -> Vec<Op> {
        self.store.operations()
    }

    /// The `first-byte-at-ms` metadata of the object at `key`.
    pub(super) fn first_byte_at(&self, key: &str) -> u64 {
        let reader = self.store.issue(Policy {
            actions: vec![Can::Get],
            prefix: format!("{BASE}/"),
        });
        let client = Store::new(self.store.clone(), self.clock.clone(), endpoint(), reader);
        let object = client.get_range(key, 0, 1).expect("read").expect("there");
        object.metadata["first-byte-at-ms"]
            .parse()
            .expect("a number")
    }
}

/// One job's uploader and the bytes the test wrote to its files.
pub(super) struct Job {
    pub(super) prefix: JobPrefix,
    pub(super) files: [PathBuf; 2],
    pub(super) written: [Vec<u8>; 2],
    pub(super) wake: Arc<ManualWake>,
    pub(super) uploader: Uploader,
}

impl Job {
    /// Appends `data` to the file of `stream`, as the capture does.
    pub(super) fn write(&mut self, stream: Stream, data: &[u8]) {
        let index = stream as usize;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.files[index])
            .expect("open");
        file.write_all(data).expect("append");
        self.written[index].extend_from_slice(data);
    }

    pub(super) fn step(&self) -> Settled {
        self.wake.step()
    }

    /// Steps until the uploader has ended, moving the clock by `by` before each step; at most
    /// `steps` steps.
    pub(super) fn run_out(&self, host: &Host, by: Duration, steps: usize) {
        let ended = (0..steps).any(|_| {
            host.advance(by);
            self.step() == Settled::Ended
        });
        assert!(ended, "the uploader did not end within {steps} steps");
    }

    pub(super) fn chunk(&self, stream: Stream, offset: u64) -> String {
        self.prefix.chunk_key(stream, offset)
    }

    /// Every object under the stream's prefix (its chunks, in a test with no marker), in
    /// order.
    pub(super) fn chunks_in(&self, host: &Host, stream: Stream) -> Vec<(String, Vec<u8>)> {
        let under = self.prefix.stream_prefix(stream);
        host.store
            .objects()
            .into_iter()
            .filter(|(key, _)| key.starts_with(&under))
            .collect()
    }
}

pub(super) fn put(key: &str, len: usize) -> Op {
    Op::Put {
        key: key.to_owned(),
        len,
    }
}

/// The puts of `ops`, as (key, length).
pub(super) fn puts(ops: &[Op]) -> Vec<(String, usize)> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Put { key, len } => Some((key.clone(), *len)),
            _ => None,
        })
        .collect()
}

pub(super) fn deletes(ops: &[Op]) -> Vec<String> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Delete { key } => Some(key.clone()),
            _ => None,
        })
        .collect()
}

/// Catches a cut off by one at either bound: a table of pending bytes and ages at 1 MiB and
/// 2 s, and one byte or millisecond either side; nothing pending is never a chunk.
#[test]
fn the_cut_is_a_pure_function() {
    let limits = CutLimits {
        chunk: MIB,
        flush: Duration::from_secs(2),
    };
    let ms = Duration::from_millis;
    let mib = MIB as u64;
    for (pending, age, want) in [
        (0, ms(0), None),
        (0, Duration::MAX, None),
        (1, ms(1999), None),
        (1, ms(2000), Some(1)),
        (1, ms(2001), Some(1)),
        (mib - 1, ms(1999), None),
        (mib - 1, ms(2000), Some(MIB - 1)),
        (mib, ms(0), Some(MIB)),
        (mib + 1, ms(0), Some(MIB)),
        (mib + 1, ms(2000), Some(MIB)),
        (5 * mib, Duration::MAX, Some(MIB)),
        (100, Duration::MAX, Some(100)),
    ] {
        assert_eq!(cut(pending, age, limits), want, "{pending} {age:?}");
    }
}

/// Catches a chunk larger than 1 MiB, an empty one, or one cut early or late: 1 MiB at once
/// is a chunk with no clock movement; 100 bytes are a chunk only once the clock has moved
/// 2 s; 2.5 MiB at once are 1 MiB, 1 MiB and, 2 s later, 0.5 MiB.
#[test]
fn a_chunk_is_cut_at_one_mebibyte_before_two_seconds_and_at_two_seconds_below_it() {
    let host = Host::new("cut-bounds");
    let mut job = host.job("cut");
    job.write(Stream::Stdout, &bytes(MIB, 1));
    job.step();
    assert_eq!(host.ops(), [put(&job.chunk(Stream::Stdout, 0), MIB)]);

    job.write(Stream::Stdout, &bytes(100, 2));
    job.step();
    host.advance(Duration::from_millis(1999));
    job.step();
    assert_eq!(host.ops().len(), 1, "not before 2 s");
    host.advance(Duration::from_millis(1));
    job.step();
    let at = MIB as u64;
    assert_eq!(host.ops()[1..], [put(&job.chunk(Stream::Stdout, at), 100)]);

    let at = at + 100;
    job.write(Stream::Stdout, &bytes(5 * MIB / 2, 3));
    job.step();
    assert_eq!(
        host.ops()[2..],
        [
            put(&job.chunk(Stream::Stdout, at), MIB),
            put(&job.chunk(Stream::Stdout, at + MIB as u64), MIB),
        ]
    );
    host.advance(Duration::from_secs(2));
    job.step();
    assert_eq!(
        host.ops()[4..],
        [put(
            &job.chunk(Stream::Stdout, at + 2 * MIB as u64),
            MIB / 2
        )]
    );
}

/// Catches an off-by-one between chunks, a swapped stream or a re-encoding: both files grow
/// in turns; each stream's chunks start where the last ended, sit under their own stream,
/// and put together are the file's bytes exactly, NULs, bytes that are not UTF-8 and a
/// last line with no newline among them.
#[test]
fn chunk_names_and_offsets_are_contiguous_and_per_stream() {
    let host = Host::new("contiguous");
    let mut job = host.job("contig");
    for round in 0..5u8 {
        let out = bytes(700 + usize::from(round) * 300_011, round);
        let err = bytes(1 + usize::from(round) * 77, round.wrapping_add(9));
        job.write(Stream::Stdout, &out);
        job.step();
        job.write(Stream::Stderr, &err);
        host.advance(Duration::from_secs(2));
        job.step();
    }
    job.write(Stream::Stdout, b"no newline at the end \xff\x00");
    job.step();
    host.advance(Duration::from_secs(2));
    job.step();
    for stream in [Stream::Stdout, Stream::Stderr] {
        let mut offset = 0u64;
        let mut joined = Vec::new();
        for (key, body) in job.chunks_in(&host, stream) {
            assert_eq!(key, job.chunk(stream, offset), "contiguous");
            assert!(!body.is_empty() && body.len() <= MIB);
            offset += body.len() as u64;
            joined.extend_from_slice(&body);
        }
        assert_eq!(joined, job.written[stream as usize], "{stream:?}");
    }
}

/// Catches a first-byte time taken from the cut or from the last byte: bytes seen at t1 and
/// more at t2 cut as one chunk carry t1; a chunk whose first byte arrived at t2 carries t2;
/// chunks put after a long outage carry the times their bytes arrived, not the put's.
#[test]
fn each_chunk_records_the_time_of_the_poll_that_first_saw_its_first_byte() {
    let host = Host::new("first-byte");
    let mut job = host.job("times");
    let t1 = epoch_ms(host.clock.now());
    job.write(Stream::Stdout, &bytes(10, 1));
    job.step();
    host.advance(Duration::from_millis(500));
    job.write(Stream::Stdout, &bytes(10, 2));
    job.step();
    host.advance(Duration::from_millis(1500));
    job.step();
    assert_eq!(host.first_byte_at(&job.chunk(Stream::Stdout, 0)), t1);

    job.write(Stream::Stdout, &bytes(10, 3));
    host.advance(Duration::from_millis(250));
    let t2 = epoch_ms(host.clock.now());
    job.step();
    host.advance(Duration::from_secs(2));
    job.step();
    assert_eq!(host.first_byte_at(&job.chunk(Stream::Stdout, 20)), t2);

    // An outage: every request fails while two runs of bytes arrive at t3 and t4.
    host.store.fail_next(3, Status::InternalError);
    job.write(Stream::Stdout, &bytes(MIB, 4));
    let t3 = epoch_ms(host.clock.now());
    job.step();
    host.advance(Duration::from_secs(61));
    job.write(Stream::Stdout, &bytes(MIB, 5));
    let t4 = epoch_ms(host.clock.now());
    job.step();
    host.advance(Duration::from_secs(61));
    job.step();
    assert_eq!(puts(&host.ops()).len(), 2, "nothing went out in the outage");
    host.advance(Duration::from_secs(61));
    job.step();
    assert_eq!(host.first_byte_at(&job.chunk(Stream::Stdout, 30)), t3);
    let second = 30 + MIB as u64;
    assert_eq!(host.first_byte_at(&job.chunk(Stream::Stdout, second)), t4);
}

/// Catches a byte skipped, repeated or sent out of order across an outage: while every
/// request fails both files grow to 3 MiB and nothing is stored; once the store answers, the
/// chunks of each stream go out in offset order and their bytes are the files'.
#[test]
fn a_store_that_is_down_keeps_the_stream_in_the_file_and_sends_it_in_order_when_it_returns() {
    let host = Host::new("store-down");
    let mut job = host.job("down");
    // The first round leaves 0.75 MiB a stream, too young and too short for a chunk; in each
    // later round the request of each stream fails.
    host.store.fail_next(6, Status::SlowDown);
    for round in 0..4u8 {
        job.write(Stream::Stdout, &bytes(3 * MIB / 4, round));
        job.write(Stream::Stderr, &bytes(3 * MIB / 4, round + 50));
        host.advance(Duration::from_secs(61));
        job.step();
        assert!(host.store.objects().is_empty(), "round {round}");
    }
    host.advance(Duration::from_secs(61));
    job.step();
    let mib = MIB as u64;
    // The streams take turns, each in offset order.
    let want: Vec<(String, usize)> = (0..3)
        .flat_map(|n| [(Stream::Stdout, n), (Stream::Stderr, n)])
        .map(|(stream, n)| (job.chunk(stream, n * mib), MIB))
        .collect();
    assert_eq!(puts(&host.ops()), want);
    for stream in [Stream::Stdout, Stream::Stderr] {
        let joined: Vec<u8> = job
            .chunks_in(&host, stream)
            .into_iter()
            .flat_map(|(_, body)| body)
            .collect();
        assert_eq!(joined, job.written[stream as usize]);
    }
}

/// Catches a marker at the wrong offset or of the wrong length, one put before the chunk it
/// follows, or a final written over a hole: the file stops at `L` while the total says
/// `L + N`; after the chunk up to `L` the store holds the marker `offset20(L).gap` with
/// `{"length":N}` and nothing at or past `L + N`; the report says `N` dropped; no final is
/// written for that stream, and the empty one writes nothing.
#[test]
fn bytes_the_capture_could_not_keep_become_a_gap_marker() {
    let host = Host::new("gap");
    let mut job = host.job("gap");
    job.write(Stream::Stdout, &bytes(100, 7));
    job.step();
    let report = job.uploader.finish((150, 0));
    assert_eq!(
        report,
        LogReport {
            stdout_bytes: 150,
            stderr_bytes: 0,
            stdout_dropped: 50,
            stderr_dropped: 0,
        }
    );
    job.run_out(&host, Duration::ZERO, 3);
    let marker = format!("{}.gap", job.chunk(Stream::Stdout, 100));
    assert_eq!(
        host.ops(),
        [put(&job.chunk(Stream::Stdout, 0), 100), put(&marker, 13)]
    );
    let objects = host.store.objects();
    assert_eq!(objects[&marker], b"{\"length\":50}");
    assert_eq!(
        objects.len(),
        2,
        "no final, nothing past the gap: {objects:?}"
    );
    assert!(job.uploader.join(), "the thread ended");
}

/// Catches a retry that re-cuts a chunk: the answer of a stored put is lost, more bytes
/// arrive, and the retry puts the same key with the same bytes; the store holds one object
/// of the first length.
#[test]
fn a_retry_after_a_lost_response_writes_an_identical_object() {
    let host = Host::new("lost-answer");
    let mut job = host.job("lost");
    job.write(Stream::Stdout, &bytes(100, 1));
    job.step();
    host.store.lose_response_of_next_put();
    host.advance(Duration::from_secs(2));
    job.step();
    let key = job.chunk(Stream::Stdout, 0);
    assert_eq!(
        host.store.objects()[&key].len(),
        100,
        "stored before the error"
    );
    job.write(Stream::Stdout, &bytes(900, 2));
    host.advance(Duration::from_secs(61));
    job.step();
    assert_eq!(host.ops(), [put(&key, 100), put(&key, 100)]);
    assert_eq!(host.store.objects()[&key], job.written[0][..100]);
    host.advance(Duration::from_secs(2));
    job.step();
    assert_eq!(host.ops()[2..], [put(&job.chunk(Stream::Stdout, 100), 900)]);
}

/// Catches an old credential used after a new one came, a byte lost across the change, a
/// state per job rather than per host, or a stale flag that never clears: with no credential
/// held nothing is put and the host is stale; a credential near its end is stale before it
/// fails; once it has expired both jobs stop and the host's one state is stale with the old
/// `until_ms`; a new grant clears it, names the new expiry, and both jobs go on in order.
#[test]
fn an_expired_host_credential_asks_for_a_new_one_and_every_uploader_switches_to_it() {
    let host = Host::bare("expiry");
    let mut first = host.job("first");
    let mut second = host.job("second");
    first.write(Stream::Stdout, &bytes(MIB, 1));
    first.step();
    assert!(host.ops().is_empty(), "no credential, no request");
    assert!(host.grant.state().stale);
    assert_eq!(host.grant.state().until_ms, None);

    let old = host.renew();
    assert!(!host.grant.state().stale);
    assert_eq!(host.grant.state().until_ms, Some(old.expires_at_ms));
    first.step();
    assert_eq!(host.ops(), [put(&first.chunk(Stream::Stdout, 0), MIB)]);

    let until = UNIX_EPOCH + Duration::from_millis(old.expires_at_ms);
    let left = until.duration_since(host.clock.now()).expect("ahead");
    host.advance(left - GRANT_STALE_MARGIN - Duration::from_secs(1));
    assert!(!host.grant.state().stale, "a second before the margin");
    host.advance(Duration::from_secs(1));
    assert!(host.grant.state().stale, "within the margin");

    host.advance(GRANT_STALE_MARGIN);
    first.write(Stream::Stdout, &bytes(MIB, 2));
    second.write(Stream::Stderr, &bytes(MIB, 3));
    first.step();
    second.step();
    assert_eq!(host.ops().len(), 1, "an expired credential sends nothing");
    assert_eq!(host.grant.state().until_ms, Some(old.expires_at_ms));
    assert!(host.grant.state().stale);

    let new = host.renew();
    assert_eq!(host.grant.state().until_ms, Some(new.expires_at_ms));
    assert!(!host.grant.state().stale);
    first.step();
    second.step();
    assert_eq!(
        host.ops()[1..],
        [
            put(&first.chunk(Stream::Stdout, MIB as u64), MIB),
            put(&second.chunk(Stream::Stderr, 0), MIB),
        ]
    );
    assert_eq!(first.chunks_in(&host, Stream::Stdout).len(), 2);
}

/// Catches a delete before the final, a chunk missed or a foreign key deleted: a job printing
/// 2.5 MiB and 100 bytes ends; `stdout.log` holds the 2.5 MiB and `stderr.log` the 100 bytes,
/// each final is put before any delete of its stream's chunks, and every chunk put is
/// deleted exactly once, nothing else.
#[test]
fn the_end_of_a_whole_stream_writes_the_final_then_deletes_the_chunks() {
    let host = Host::new("finals");
    let mut job = host.job("finals");
    job.write(Stream::Stdout, &bytes(5 * MIB / 2, 1));
    job.write(Stream::Stderr, &bytes(100, 2));
    job.step();
    job.uploader.finish((5 * MIB as u64 / 2, 100));
    job.run_out(&host, Duration::ZERO, 3);
    let objects = host.store.objects();
    let final_of = |stream| job.prefix.final_key(stream);
    assert_eq!(objects[&final_of(Stream::Stdout)], job.written[0]);
    assert_eq!(objects[&final_of(Stream::Stderr)], job.written[1]);
    assert_eq!(objects.len(), 2, "only the finals are left: {objects:?}");
    let ops = host.ops();
    for stream in [Stream::Stdout, Stream::Stderr] {
        let under = job.prefix.stream_prefix(stream);
        let final_at = ops
            .iter()
            .position(|op| matches!(op, Op::Put { key, .. } if *key == final_of(stream)))
            .expect("a final");
        let chunks: Vec<String> = puts(&ops)
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with(&under))
            .collect();
        let mut deleted: Vec<String> = deletes(&ops)
            .into_iter()
            .filter(|key| key.starts_with(&under))
            .collect();
        deleted.sort();
        assert_eq!(deleted, chunks, "each chunk deleted once");
        let first_delete = ops
            .iter()
            .position(|op| matches!(op, Op::Delete { key } if key.starts_with(&under)))
            .expect("a delete");
        assert!(first_delete > final_at, "a delete before the final");
    }
    assert_eq!(deletes(&ops).len(), 4, "3 stdout chunks, 1 stderr chunk");
}

/// Catches chunks deleted with no final: the final's put fails every time until the budget
/// is spent; no delete is sent and every chunk is still there.
#[test]
fn a_failed_final_deletes_nothing() {
    let mut host = Host::new("final-fails");
    host.limits.budget = Duration::from_secs(300);
    let mut job = host.job("final-fails");
    job.write(Stream::Stdout, &bytes(MIB + 10, 1));
    job.step();
    host.advance(Duration::from_secs(2));
    job.step();
    assert_eq!(host.store.objects().len(), 2);
    host.store.fail_next(1000, Status::InternalError);
    job.uploader.finish((MIB as u64 + 10, 0));
    job.run_out(&host, Duration::from_secs(60), 10);
    assert!(deletes(&host.ops()).is_empty());
    assert_eq!(host.store.objects().len(), 2, "both chunks are kept");
}

/// Catches a final over a hole or over the one-put limit, and a stream that loses its final
/// for the other's sake: a stream with dropped bytes, and (with the limit at 1000 bytes) a
/// whole stream of 2000 bytes, keep their chunks and the marker and get no final, while the
/// other, short, stream of each job gets its final.
#[test]
fn a_stream_the_cap_cut_or_over_the_put_limit_keeps_its_chunks() {
    let mut host = Host::new("no-final");
    host.limits.max_put = 1000;
    let mut over = host.job("over");
    over.write(Stream::Stdout, &bytes(2000, 1));
    over.write(Stream::Stderr, &bytes(10, 2));
    over.step();
    over.uploader.finish((2000, 10));
    over.run_out(&host, Duration::ZERO, 3);
    let mut short = host.job("short");
    short.write(Stream::Stdout, &bytes(300, 3));
    short.write(Stream::Stderr, &bytes(10, 4));
    short.uploader.finish((500, 10));
    short.run_out(&host, Duration::ZERO, 3);
    let objects = host.store.objects();
    assert_eq!(objects[&over.chunk(Stream::Stdout, 0)], over.written[0]);
    assert!(!objects.contains_key(&over.prefix.final_key(Stream::Stdout)));
    assert_eq!(
        objects[&over.prefix.final_key(Stream::Stderr)],
        over.written[1]
    );
    assert_eq!(objects[&short.chunk(Stream::Stdout, 0)], short.written[0]);
    let marker = format!("{}.gap", short.chunk(Stream::Stdout, 300));
    assert_eq!(objects[&marker], b"{\"length\":200}");
    assert!(!objects.contains_key(&short.prefix.final_key(Stream::Stdout)));
    assert_eq!(
        objects[&short.prefix.final_key(Stream::Stderr)],
        short.written[1]
    );
    let deleted = deletes(&host.ops());
    assert_eq!(
        deleted,
        [
            over.chunk(Stream::Stderr, 0),
            short.chunk(Stream::Stderr, 0)
        ],
        "only the chunks of streams with a final"
    );
}

/// Catches an uploader that reads the files by path after the end: they are removed (as
/// `Runtime::remove` removes the lease directory) right after `finish` while the store holds
/// every put, and once it lets them go the final and the deletes complete with every byte.
#[test]
fn the_final_is_written_after_the_lease_directory_is_removed() {
    let host = Host::new("removed");
    let mut job = host.job("removed");
    job.write(Stream::Stdout, &bytes(3 * MIB / 2, 1));
    job.write(Stream::Stderr, &bytes(64, 2));
    job.uploader.finish((3 * MIB as u64 / 2, 64));
    host.store.block_puts();
    std::thread::scope(|scope| {
        let stepped = scope.spawn(|| job.step());
        host.store.wait_for_held_puts(1);
        for file in &job.files {
            std::fs::remove_file(file).expect("removed");
        }
        host.store.release_puts();
        assert_eq!(stepped.join().expect("stepped"), Settled::Ended);
    });
    let objects = host.store.objects();
    assert_eq!(
        objects[&job.prefix.final_key(Stream::Stdout)],
        job.written[0]
    );
    assert_eq!(
        objects[&job.prefix.final_key(Stream::Stderr)],
        job.written[1]
    );
    assert_eq!(objects.len(), 2);
}

/// Catches an uploader that outlives its budget or leaves its thread: with every request
/// failing it still runs a second before `FINISH_BUDGET` after the end, and ends at it; the
/// thread is joined.
#[test]
fn the_uploader_gives_up_after_its_budget() {
    let host = Host::new("budget");
    let mut job = host.job("budget");
    job.write(Stream::Stdout, &bytes(10, 1));
    host.store.fail_next(1000, Status::SlowDown);
    job.uploader.finish((10, 0));
    assert_eq!(job.step(), Settled::Asleep);
    host.advance(super::FINISH_BUDGET - Duration::from_secs(1));
    assert_eq!(job.step(), Settled::Asleep);
    host.advance(Duration::from_secs(1));
    assert_eq!(job.step(), Settled::Ended);
    assert!(job.uploader.join());
    assert!(host.store.objects().is_empty());
    assert_eq!(job.wake.notified(), 1, "the end woke it");
}

/// Catches an end that goes on with a credential the store called expired, or one that gives
/// up although a newer one came: an expired answer during the end with no newer credential
/// ends the upload at once, with no final and no delete; with a new grant that arrived while
/// the request was out, the end retries with it and completes.
#[test]
fn the_finish_stops_at_an_expired_credential_with_no_replacement_and_continues_with_a_new_one() {
    let host = Host::new("finish-expiry");
    let mut job = host.job("given-up");
    job.write(Stream::Stdout, &bytes(10, 1));
    job.uploader.finish((10, 0));
    host.store.fail_next(1, Status::ExpiredToken);
    assert_eq!(job.step(), Settled::Ended);
    assert!(host.ops().is_empty());
    assert!(host.grant.state().stale, "the refusal is reported");

    let host = Host::new("finish-renewed");
    let mut job = host.job("renewed");
    job.write(Stream::Stdout, &bytes(10, 1));
    job.uploader.finish((10, 0));
    host.store.block_puts();
    std::thread::scope(|scope| {
        let stepped = scope.spawn(|| job.step());
        host.store.wait_for_held_puts(1);
        host.store.fail_next(1, Status::ExpiredToken);
        host.renew();
        host.store.release_puts();
        assert_eq!(stepped.join().expect("stepped"), Settled::Ended);
    });
    let objects = host.store.objects();
    assert_eq!(
        objects[&job.prefix.final_key(Stream::Stdout)],
        job.written[0]
    );
    assert_eq!(objects.len(), 1, "the chunk was deleted");
    assert!(!host.grant.state().stale);
}

/// Catches a delete retried for ever, or a failed delete that stops the others: with a
/// credential that may put and not delete, the first chunk's delete is refused three times
/// and left; with one that may delete, the second chunk is deleted and the upload ends.
#[test]
fn a_delete_that_keeps_failing_is_left_after_three_tries() {
    let host = Host::new("delete-fails");
    let mut job = host.job("deletes");
    job.write(Stream::Stdout, &bytes(MIB + 5, 1));
    job.step();
    host.advance(Duration::from_secs(2));
    job.step();
    host.renew_with(vec![Can::Put]);
    job.uploader.finish((MIB as u64 + 5, 0));
    for _ in 0..3 {
        assert_eq!(job.step(), Settled::Asleep);
        host.advance(Duration::from_secs(61));
    }
    host.renew();
    assert_eq!(job.step(), Settled::Ended);
    let objects = host.store.objects();
    assert!(objects.contains_key(&job.prefix.final_key(Stream::Stdout)));
    assert!(objects.contains_key(&job.chunk(Stream::Stdout, 0)), "left");
    assert!(!objects.contains_key(&job.chunk(Stream::Stdout, MIB as u64)));
    assert_eq!(
        deletes(&host.ops()),
        [job.chunk(Stream::Stdout, MIB as u64)]
    );
}

/// Catches a stream given up on a file that cannot be read: a chunk whose bytes are gone
/// from the file (it was cut short) is not put, and the upload waits as on a failed request.
#[test]
fn a_chunk_the_file_no_longer_holds_is_not_put() {
    let host = Host::new("short-file");
    let mut job = host.job("short");
    job.write(Stream::Stdout, &bytes(100, 1));
    job.step();
    File::create(&job.files[0]).expect("cut short");
    host.advance(Duration::from_secs(2));
    assert_eq!(job.step(), Settled::Asleep);
    assert!(host.ops().is_empty());
}

/// Catches a job with nothing printed that writes objects: both streams empty at the end, the
/// upload ends at once and the store holds nothing.
#[test]
fn an_empty_job_writes_no_object() {
    let host = Host::new("empty");
    let job = host.job("empty");
    assert_eq!(
        job.uploader.finish((0, 0)),
        LogReport {
            stdout_bytes: 0,
            stderr_bytes: 0,
            stdout_dropped: 0,
            stderr_dropped: 0,
        }
    );
    assert_eq!(job.step(), Settled::Ended);
    assert!(host.ops().is_empty());
}

/// A writer that keeps what is logged.
#[derive(Clone, Default)]
pub(super) struct Lines(Arc<Mutex<Vec<u8>>>);

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
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lines")).into_owned()
    }

    /// Collects every event of every level, on this thread and the uploaders' it starts.
    pub(super) fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        guard
    }
}

/// A transport that panics on its first request.
pub(super) struct Panicking;

impl js_logstore::Transport for Panicking {
    fn send(
        &self,
        _: js_logstore::HttpRequest,
    ) -> Result<js_logstore::HttpResponse, js_logstore::TransportError> {
        panic!("a transport fault")
    }
}

/// Catches a panic that escapes the uploader's thread or is not reported: the transport
/// panics on the first put; the thread ends, one warning names the job's prefix, and the
/// report of the end is still given.
#[test]
fn a_panic_in_the_uploader_ends_its_thread_with_one_warning() {
    let lines = Lines::default();
    let _guard = lines.capture();
    let host = Host::new("panic");
    let (wake, mut logs) = host.logs_for_test();
    logs.transport = Arc::new(Panicking);
    let mut job = host.job_with("panics", (wake, logs));
    job.write(Stream::Stdout, &bytes(MIB, 1));
    assert_eq!(job.step(), Settled::Ended);
    assert_eq!(job.uploader.finish((MIB as u64, 0)).stdout_dropped, 0);
    assert!(job.uploader.join(), "the panic was caught in the thread");
    std::io::Write::flush(&mut lines.clone()).expect("flushed");
    let text = lines.text();
    assert_eq!(text.matches("WARN").count(), 1, "{text}");
    assert!(text.contains("js/panics/"), "{text}");
}

/// Catches a secret in a log line, an error or a `Debug` text: every failure the store can
/// answer and a full run are logged at every level; no line, and no `Debug` of the cell, the
/// settings or the uploader, holds the secret key, the session token or a signature.
#[test]
fn no_secret_reaches_a_log_or_an_error() {
    let lines = Lines::default();
    let _guard = lines.capture();
    let host = Host::new("secrets");
    let grant = host.renew();
    let mut job = host.job("secrets");
    for (round, status) in [
        Status::SlowDown,
        Status::InternalError,
        Status::TooManyRequests,
        Status::AccessDenied,
        Status::ExpiredToken,
    ]
    .into_iter()
    .enumerate()
    {
        job.write(Stream::Stdout, &bytes(10, round as u8));
        job.step();
        host.advance(Duration::from_secs(61));
        host.store.fail_next(1, status);
        job.step();
    }
    host.renew();
    job.uploader.finish((50, 0));
    job.run_out(&host, Duration::from_secs(61), 5);
    let debug = format!(
        "{:?} {:?} {:?}",
        host.grant,
        host.logs(ManualWake::new()),
        job.uploader
    );
    let text = format!("{}{debug}", lines.text());
    assert!(
        text.contains("live logs"),
        "the failures are logged: {text}"
    );
    for secret in [
        grant.secret_access_key.as_str(),
        grant.session_token.as_deref().expect("a token"),
        "fake-secret-",
        "fake-session-token-",
        "Signature=",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

/// Catches a grant that resets the refusal when the server resends it, or a refusal of an old
/// credential that marks the new one: the same grant again keeps the generation and the
/// stale flag; a refusal of an older generation changes nothing.
#[test]
fn the_cell_keeps_one_credential_per_generation() {
    let host = Host::new("generations");
    let first = host.grant.current().expect("held");
    assert_eq!(first.1, 1);
    host.grant.refused(1);
    let resent = grant_of(&first.0);
    host.grant.replace(&resent);
    assert_eq!(host.grant.generation(), 1, "the very grant again");
    assert!(host.grant.state().stale, "still refused");
    host.renew();
    host.grant.refused(1);
    assert_eq!(host.grant.generation(), 2);
    assert!(!host.grant.state().stale, "an old refusal changes nothing");
}

/// Catches a runtime that starts an uploader where none is asked for, gives a run without a
/// sink the larger cap, or reports logs without an uploader; and an uploader whose files are
/// not there, which leaves the run without one.
#[test]
fn a_run_holds_live_logs_only_with_a_sink_and_settings() {
    let host = Host::new("run-logs");
    let (_wake, logs) = host.logs_for_test();
    let sink = Sink::new(endpoint(), JobPrefix::new(BASE, "run").expect("prefix"));
    let missing = host.dir.join("missing");
    let paths = (missing.as_path(), missing.as_path());
    for (logs, sink, cap) in [
        (None, None, None),
        (None, Some(&sink), None),
        (Some(&logs), None, None),
        (Some(&logs), Some(&sink), Some(1 << 20)),
    ] {
        let mut run = RunLogs::asked(logs, sink);
        assert_eq!(run.file_cap(logs), cap);
        run.start(logs, paths);
        run.end((10, 10));
        assert_eq!(run.report(), None, "no uploader, no report");
    }
    // The warning of a start that failed, read by a subscriber this time.
    let lines = Lines::default();
    let guard = lines.capture();
    RunLogs::asked(Some(&logs), Some(&sink)).start(Some(&logs), paths);
    drop(guard);
    assert!(lines.text().contains("did not start"), "{}", lines.text());
    assert!(Uploader::start(&sink, paths, &logs).is_err());
    // Standard output there, standard error not.
    let stdout = host.dir.join("stdout-only");
    File::create(&stdout).expect("stdout");
    let half = (stdout.as_path(), missing.as_path());
    assert!(Uploader::start(&sink, half, &logs).is_err());
}

/// Catches a production wake that sleeps through the end or past its time: a notified wake
/// returns from a long sleep at once, and a sleep whose time has passed returns at once; the
/// production settings make such a wake and share the cell they were given.
#[test]
fn the_clock_wake_returns_when_notified_or_due() {
    let cell = Arc::new(HostGrant::new(Arc::new(js_logstore::SystemClock)));
    let logs = LiveLogs::system(super::DEFAULT_FILE_CAP, cell.clone());
    assert!(Arc::ptr_eq(&logs.grant, &cell));
    assert_eq!(logs.limits, Limits::default());
    let wake = (logs.wakes)();
    wake.notify();
    wake.sleep_until(SystemTime::now() + Duration::from_secs(3600));
    wake.sleep_until(UNIX_EPOCH);
    wake.ended();
    let plan = super::LogPlan::new(super::DEFAULT_FILE_CAP);
    assert_eq!(plan, plan.clone());
    assert_ne!(plan, super::LogPlan::new(super::DEFAULT_FILE_CAP));
    let wider = super::LogPlan {
        file_cap: plan.file_cap + 1,
        grant: plan.grant.clone(),
    };
    assert_ne!(plan, wider);
    assert_eq!(plan.live().file_cap, super::DEFAULT_FILE_CAP);
    assert!(Arc::ptr_eq(&plan.live().grant, &plan.grant));
}
