//! The hand-written job commands, `job wait`, `job follow` and `job run`, against the
//! in-memory job server of `fake_jobs` (scripted answers, and for `job run`'s submit the
//! reference job service behind it), with an injected waiter that records each pause and
//! returns at once and an injected signal source the test fires from inside a pause: no
//! port, no clock, no real signal.

use std::ffi::OsString;
use std::future::{Future, ready};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_cli::{ForwardedSignal, SignalSource, Signaller};
use js_client::jobs::Waiter;
use js_conformance::jobs::ReferenceJobs;
use js_proto::v1::{
    CancelJobRequest, GetJobRequest, GetJobResponse, GetLogsRequest, GetLogsResponse,
    GetResultResponse, Job, JobResult, JobState, LogGap, LogStream, RefusalCode, ResultReason,
    SubmitJobRequest,
};
use prost::Message as _;
use tonic::Code;

use crate::fake_jobs::{Answer, Fake};
use crate::support::{Outcome, Scratch, TOKEN, services};

/// A loopback origin over plain http; the in-memory dialer never dials it.
const PLAIN: &str = "http://localhost:7070";

/// A pinned image the reference service accepts.
const IMAGE: &str =
    "registry.example/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The pause between two reads of a live job, and between two tries of a submit.
const TWO: Duration = Duration::from_secs(2);

/// The path of a call of the job service.
fn call(rpc: &str) -> String {
    format!("/jobscheduler.v1.JobService/{rpc}")
}

/// What a test does at one pause: the pause's number, from 0, and the act.
type Act = (usize, Box<dyn Fn() + Send>);

/// A waiter that records each pause and returns at once, after doing what the test asked
/// of that pause.
#[derive(Default)]
struct Paces {
    asked: Mutex<Vec<Duration>>,
    at: Mutex<Vec<Act>>,
}

impl Paces {
    fn asked(&self) -> Vec<Duration> {
        self.asked.lock().unwrap().clone()
    }

    /// Does `act` at pause number `pause`.
    fn at(&self, pause: usize, act: impl Fn() + Send + 'static) {
        self.at.lock().unwrap().push((pause, Box::new(act)));
    }
}

impl Waiter for Paces {
    fn wait(&self, interval: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let number = {
            let mut asked = self.asked.lock().unwrap();
            asked.push(interval);
            asked.len() - 1
        };
        for (pause, act) in self.at.lock().unwrap().iter() {
            if *pause == number {
                act();
            }
        }
        Box::pin(ready(()))
    }
}

/// The handle `js` hands its signal source, once it has: a test fires signals through it.
#[derive(Clone, Default)]
struct Signals(Arc<Mutex<Option<Signaller>>>);

impl Signals {
    /// The source that hands the handle here.
    fn source(&self) -> SignalSource {
        let slot = self.0.clone();
        Box::new(move |signaller| {
            *slot.lock().unwrap() = Some(signaller);
            Ok(())
        })
    }

    /// Fires `signal`, as the program's handler would on a delivery.
    fn fire(&self, signal: ForwardedSignal) {
        self.0.lock().unwrap().as_ref().unwrap().deliver(signal);
    }
}

/// The arguments of `js --server PLAIN --token-file <token> <words>`, with an owner-only
/// token file.
fn args(scratch: &Scratch, words: &[&str]) -> Vec<OsString> {
    with_token(scratch, 0o600, words)
}

/// The arguments of `js --server PLAIN --token-file <token> <words>`, with a token file of
/// `mode`.
fn with_token(scratch: &Scratch, mode: u32, words: &[&str]) -> Vec<OsString> {
    let token = scratch.write("token", TOKEN, mode);
    let mut args: Vec<OsString> = vec![
        "js".into(),
        "--server".into(),
        PLAIN.into(),
        "--token-file".into(),
        token.into(),
    ];
    args.extend(words.iter().map(OsString::from));
    args
}

/// Runs `words` against `fake`, pausing on `paces`, with `source` for its signals.
fn js(
    scratch: &Scratch,
    fake: &Fake,
    paces: &Arc<Paces>,
    source: SignalSource,
    words: &[&str],
) -> Outcome {
    let services = js_cli::Services::new(source)
        .with_dialer(fake.dialer())
        .with_job_waiter(paces.clone());
    crate::support::run(args(scratch, words), services)
}

/// Runs `words` against `fake` with no signals.
fn quiet(scratch: &Scratch, fake: &Fake, paces: &Arc<Paces>, words: &[&str]) -> Outcome {
    js(scratch, fake, paces, Box::new(|_| Ok(())), words)
}

/// An answer of `GetJob`: job `j-1` in `state`.
fn job(state: JobState) -> Answer {
    let answer = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            state: state.into(),
            ..Job::default()
        }),
    };
    Answer::Message(answer.encode_to_vec())
}

/// An answer of `GetResult`: a failed job's result, for `reason`.
fn result(reason: ResultReason) -> Answer {
    let answer = GetResultResponse {
        result: Some(JobResult {
            state: JobState::Failed.into(),
            reason: reason.into(),
            ..JobResult::default()
        }),
    };
    Answer::Message(answer.encode_to_vec())
}

/// An answer of `GetLogs`: `data` from `offset`, the end of the stream when `eof`.
fn page(offset: u64, data: &[u8], eof: bool) -> GetLogsResponse {
    GetLogsResponse {
        state: JobState::Running.into(),
        data: data.to_vec(),
        offset,
        next_offset: offset + u64::try_from(data.len()).unwrap(),
        eof,
        ..GetLogsResponse::default()
    }
}

/// The requests of `rpc` that `fake` read, decoded as `T`.
fn sent<T: prost::Message + Default>(fake: &Fake, rpc: &str) -> Vec<T> {
    let path = call(rpc);
    let seen = fake.seen().into_iter().filter(|seen| seen.path == path);
    seen.map(|seen| T::decode(seen.message.as_slice()).unwrap())
        .collect()
}

/// The calls `fake` read, by name, in order.
fn calls(fake: &Fake) -> Vec<String> {
    let names = fake.seen().into_iter();
    names
        .map(|seen| seen.path.rsplit('/').next().unwrap().to_owned())
        .collect()
}

/// Asserts that `job wait` on a job read as running, running, then succeeded exits 0
/// after asking the waiter for two pauses of two seconds, prints `job j-1 succeeded` and
/// reads only that job. Catches a wait that ends at the first read, one that polls without
/// the waiter, and a success that exits nonzero.
#[test]
fn job_wait_exits_0_after_two_pauses_when_the_job_succeeds() {
    let scratch = Scratch::new("wait_ok");
    let fake = Fake::new();
    fake.answer(
        "GetJob",
        vec![
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Succeeded),
        ],
    );
    let paces = Arc::new(Paces::default());
    let got = quiet(&scratch, &fake, &paces, &["job", "wait", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "job j-1 succeeded\n", "")
    );
    assert_eq!(paces.asked(), [TWO, TWO]);
    let reads: Vec<GetJobRequest> = sent(&fake, "GetJob");
    assert_eq!(reads.len(), 3);
    assert!(reads.iter().all(|read| read.job_id == "j-1"));
    assert_eq!(calls(&fake), ["GetJob", "GetJob", "GetJob"]);
}

/// Asserts the exit status and the printed end of every end but success: failed (by its
/// exit code, or for its memory) 10, cancelled 11, timed out (failed at its wall limit) 12,
/// lost 13, failed to start 14, each distinct; and the line in JSON. Catches two ends that
/// share a status, and a job stopped at its wall limit reported as merely failed.
#[test]
fn job_wait_gives_each_end_its_own_status() {
    let scratch = Scratch::new("wait_ends");
    let cases = [
        (JobState::Failed, Some(ResultReason::Exit), 10, "failed"),
        (JobState::Failed, Some(ResultReason::Memory), 10, "failed"),
        (JobState::Cancelled, None, 11, "cancelled"),
        (
            JobState::Failed,
            Some(ResultReason::WallLimit),
            12,
            "timed-out",
        ),
        (JobState::Lost, None, 13, "lost"),
        (JobState::FailedToStart, None, 14, "failed-to-start"),
    ];
    for (state, reason, code, name) in cases {
        let fake = Fake::new();
        fake.answer("GetJob", vec![job(state)]);
        fake.answer("GetResult", reason.into_iter().map(result).collect());
        let paces = Arc::new(Paces::default());
        let got = quiet(&scratch, &fake, &paces, &["job", "wait", "--job-id", "j-1"]);
        let line = format!("job j-1 {name}\n");
        assert_eq!(
            (got.code, got.out.as_str(), got.err.as_str()),
            (code, line.as_str(), ""),
            "{state:?} {reason:?}"
        );
    }
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Failed)]);
    fake.answer("GetResult", vec![result(ResultReason::WallLimit)]);
    let paces = Arc::new(Paces::default());
    let words = ["job", "wait", "--job-id", "j-1", "--json"];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str()),
        (12, "{\"id\":\"j-1\",\"outcome\":\"timed-out\"}\n")
    );
}

/// Asserts that a read that fails mid-wait, and a job in a state this `js` does not know,
/// exit 15 (`undetermined`) with the whole reason on stderr, in text and in JSON, and print
/// no end. Catches a failed call or an unknown state reported as one of the job's ends.
#[test]
fn job_wait_is_undetermined_when_a_call_fails_or_the_state_is_unknown() {
    let scratch = Scratch::new("wait_unknown");
    let fake = Fake::new();
    fake.answer(
        "GetJob",
        vec![
            job(JobState::Running),
            Answer::Status(Code::Unavailable, "down"),
        ],
    );
    let paces = Arc::new(Paces::default());
    let got = quiet(&scratch, &fake, &paces, &["job", "wait", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            15,
            "",
            "js: cannot tell how job j-1 ended: no answer from http://localhost:7070 after the request was sent: down\n"
        )
    );

    let fake = Fake::new();
    let newer = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            state: 99,
            ..Job::default()
        }),
    };
    fake.answer("GetJob", vec![Answer::Message(newer.encode_to_vec())]);
    let words = ["job", "wait", "--job-id", "j-1", "--json"];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            15,
            "",
            "{\"error\":\"undetermined\",\"message\":\"cannot tell how job j-1 ended: job j-1 is in a state this client does not know (99)\"}\n"
        )
    );
}

/// Asserts that `job follow` writes the stream's pages, read across calls whose offsets
/// advance by each answer's `next_offset`, to stdout as their concatenation exactly once;
/// that a page saying the job ended but not `eof` is read past; that an empty page is
/// followed by a one-second pause; that every read follows stdout; and that it exits 0 at
/// `eof`. Catches bytes written twice or dropped at a page boundary, and a follow that stops
/// at the first sight of the end with bytes unread.
#[test]
fn job_follow_writes_each_byte_once_and_ends_only_at_eof() {
    let scratch = Scratch::new("follow_once");
    let mut ended = page(5, b" ", false);
    ended.state = JobState::Succeeded.into();
    let pages = [
        page(0, b"hel", false),
        page(3, b"lo", false),
        page(5, b"", false),
        ended,
        page(6, b"world", true),
    ];
    let fake = Fake::new();
    let answers = pages
        .iter()
        .map(|page| Answer::Message(page.encode_to_vec()));
    fake.answer("GetLogs", answers.collect());
    let paces = Arc::new(Paces::default());
    let got = quiet(
        &scratch,
        &fake,
        &paces,
        &["job", "follow", "--job-id", "j-1"],
    );
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "hello world", "")
    );
    let reads: Vec<GetLogsRequest> = sent(&fake, "GetLogs");
    let offsets: Vec<u64> = reads.iter().map(|read| read.from_offset.unwrap()).collect();
    assert_eq!(offsets, [0, 3, 5, 5, 6]);
    assert!(reads.iter().all(|read| read.follow == Some(true)
        && read.stream == i32::from(LogStream::Stdout)
        && read.job_id == "j-1"));
    assert_eq!(paces.asked(), [Duration::from_secs(1)]);
}

/// Asserts that `job follow --stream stderr` reads stderr, writes its data to stdout, and
/// says each gap on stderr (in text, and in JSON), and that a follow the server refuses as
/// busy is tried again after five seconds. Catches a gap dropped without a word, the flag
/// read as the other stream, and a busy refusal that ends the follow.
#[test]
fn job_follow_reads_the_stream_named_and_says_its_gaps() {
    let scratch = Scratch::new("follow_gaps");
    let mut gapped = page(4, b"tail", true);
    gapped.gaps = vec![LogGap {
        offset: 0,
        length: 4,
    }];
    for (json, said) in [
        (false, "js: stderr bytes 0 to 4 are not in the log store\n"),
        (
            true,
            "{\"gap\":{\"length\":4,\"offset\":0,\"stream\":\"stderr\"}}\n",
        ),
    ] {
        let fake = Fake::new();
        fake.answer(
            "GetLogs",
            vec![
                Answer::Status(Code::ResourceExhausted, "too-many-follows"),
                Answer::Message(gapped.encode_to_vec()),
            ],
        );
        let paces = Arc::new(Paces::default());
        let mut words = vec!["job", "follow", "--job-id", "j-1", "--stream", "stderr"];
        if json {
            words.push("--json");
        }
        let got = quiet(&scratch, &fake, &paces, &words);
        assert_eq!(
            (got.code, got.out.as_str(), got.err.as_str()),
            (0, "tail", said)
        );
        let reads: Vec<GetLogsRequest> = sent(&fake, "GetLogs");
        assert_eq!(reads.len(), 2);
        assert!(
            reads
                .iter()
                .all(|read| read.stream == i32::from(LogStream::Stderr)
                    && read.from_offset == Some(0))
        );
        assert_eq!(paces.asked(), [Duration::from_secs(5)]);
    }
}

/// Asserts that `job follow` sends a read whose answer is lost again from the same offset
/// after two seconds, and that the third loss in a row ends it with exit 9 (`unconfirmed`)
/// and the call's reason. Catches a transient log store failure that cuts the stream short,
/// and a follow retried forever against a server that is gone.
#[test]
fn job_follow_sends_a_lost_read_again_three_times_in_a_row_at_most() {
    let scratch = Scratch::new("follow_lost");
    let fake = Fake::new();
    fake.answer(
        "GetLogs",
        vec![
            Answer::Status(Code::Unavailable, "log-store-unavailable"),
            Answer::Status(Code::Unavailable, "log-store-unavailable"),
            Answer::Message(page(0, b"abc", true).encode_to_vec()),
        ],
    );
    let paces = Arc::new(Paces::default());
    let words = ["job", "follow", "--job-id", "j-1"];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "abc", "")
    );
    let reads: Vec<GetLogsRequest> = sent(&fake, "GetLogs");
    let offsets: Vec<u64> = reads.iter().map(|read| read.from_offset.unwrap()).collect();
    assert_eq!(offsets, [0, 0, 0]);
    assert_eq!(paces.asked(), [TWO, TWO]);

    let fake = Fake::new();
    fake.answer(
        "GetLogs",
        vec![Answer::Status(Code::Unavailable, "log-store-unavailable")],
    );
    let paces = Arc::new(Paces::default());
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            9,
            "",
            "js: no answer from http://localhost:7070 after the request was sent: log-store-unavailable\n"
        )
    );
    assert_eq!(sent::<GetLogsRequest>(&fake, "GetLogs").len(), 3);
    assert_eq!(paces.asked(), [TWO, TWO]);
}

/// A writer that refuses every write, as a closed pipe does.
struct Closed;

impl io::Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that a follow the server refuses (no live logs) exits 3 with the refusal, as any
/// job command's refusal does, and that a stdout that cannot be written exits 1 saying so.
/// Catches a refusal retried forever, and bytes dropped silently when they cannot be
/// written.
#[test]
fn job_follow_ends_on_a_refusal_or_a_closed_stdout() {
    let scratch = Scratch::new("follow_fails");
    let fake = Fake::new();
    fake.answer(
        "GetLogs",
        vec![Answer::Status(Code::FailedPrecondition, "no-live-logs")],
    );
    let paces = Arc::new(Paces::default());
    let got = quiet(
        &scratch,
        &fake,
        &paces,
        &["job", "follow", "--job-id", "j-1"],
    );
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (3, "", "js: refused: no-live-logs\n")
    );

    let fake = Fake::new();
    fake.answer(
        "GetLogs",
        vec![Answer::Message(page(0, b"abc", true).encode_to_vec())],
    );
    let services = services()
        .with_dialer(fake.dialer())
        .with_job_waiter(paces.clone());
    let mut err = Vec::new();
    let words = ["job", "follow", "--job-id", "j-1"];
    let code = js_cli::run_with(args(&scratch, &words), &mut Closed, &mut err, services);
    assert_eq!(
        (code, String::from_utf8(err).unwrap().as_str()),
        (1, "js: cannot write the log: closed\n")
    );
}

/// Answers each `GetLogs` of `job run` by its stream and offset from `streams` (stdout's
/// pages, then stderr's), whatever order the two follows read in.
fn by_stream(fake: &Fake, streams: [Vec<GetLogsResponse>; 2]) {
    fake.respond("GetLogs", move |message| {
        let read = GetLogsRequest::decode(message).unwrap();
        let pages = match LogStream::try_from(read.stream).unwrap() {
            LogStream::Stdout => &streams[0],
            _ => &streams[1],
        };
        let from = read.from_offset.unwrap();
        let found = pages.iter().find(|page| page.offset == from).unwrap();
        Answer::Message(found.encode_to_vec())
    });
}

/// Both streams of a job that wrote nothing: at their end at once.
fn silent(fake: &Fake) {
    by_stream(fake, [vec![page(0, b"", true)], vec![page(0, b"", true)]]);
}

/// Asserts that when the answer to `job run`'s submit is lost after the service made the
/// job, the submit is sent again with the same drawn id (printed first) and the same bytes
/// after a two-second pause, and the service holds one job; and that the run then exits
/// with the job's status. Catches a retry that draws a new id (a second job) and a run that
/// gives up on the first lost answer.
#[test]
fn job_run_sends_a_lost_submit_again_with_its_id_and_makes_one_job() {
    let scratch = Scratch::new("run_retry");
    let jobs = ReferenceJobs::new("default");
    let fake = Fake::serving(jobs);
    fake.answer("SubmitJob", vec![Answer::BackedThenLost, Answer::Backed]);
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    silent(&fake);
    let paces = Arc::new(Paces::default());
    let words = [
        "job",
        "run",
        "--spec.image",
        IMAGE,
        "--spec.command",
        "true",
        "--spec.wall-limit-seconds",
        "10m",
    ];
    let got = quiet(&scratch, &fake, &paces, &words);
    let submits: Vec<SubmitJobRequest> = sent(&fake, "SubmitJob");
    assert_eq!(submits.len(), 2);
    assert_eq!(submits[0], submits[1]);
    let id = submits[0].spec.clone().unwrap().job_id;
    assert_eq!(id.len(), 32, "{id}");
    let want = format!("js: job id {id}\njob {id} succeeded\n");
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "", want.as_str())
    );
    assert_eq!(paces.asked(), [TWO]);

    let listed = quiet(&scratch, &fake, &paces, &["job", "list", "--json"]);
    assert_eq!(listed.code, 0, "{}", listed.err);
    let value: serde_json::Value = serde_json::from_str(&listed.out).unwrap();
    let ids: Vec<&str> = value["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [id.as_str()]);
}

/// Asserts that `job run` with `--id` submits that id (printing no drawn one), writes the
/// job's stdout to stdout and its stderr to stderr, each in order and once, and exits with
/// the status `job wait` gives the job's end (here timed out, 12), printing the end on stderr
/// after both streams. Catches a run that exits 0 whatever the job did, streams crossed or
/// cut short when the job ends, and an end printed before the streams are drained.
#[test]
fn job_run_writes_both_streams_and_exits_with_the_jobs_status() {
    let scratch = Scratch::new("run_streams");
    let fake = Fake::new();
    fake.answer(
        "GetJob",
        vec![job(JobState::Running), job(JobState::Failed)],
    );
    fake.answer("GetResult", vec![result(ResultReason::WallLimit)]);
    by_stream(
        &fake,
        [
            vec![page(0, b"out1 ", false), page(5, b"out2", true)],
            vec![page(0, b"err", true)],
        ],
    );
    let paces = Arc::new(Paces::default());
    let words = ["job", "run", "--id", "nightly-7", "--spec.image", IMAGE];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (12, "out1 out2", "errjob nightly-7 timed-out\n")
    );
    let submits: Vec<SubmitJobRequest> = sent(&fake, "SubmitJob");
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0].spec.clone().unwrap().job_id, "nightly-7");
    let reads: Vec<GetJobRequest> = sent(&fake, "GetJob");
    assert!(reads.iter().all(|read| read.job_id == "nightly-7"));
    assert_eq!(paces.asked(), [TWO]);
}

/// Asserts that when the job has ended while its stdout still has bytes the log store has
/// not yet given (the fake answers `GetLogs` with no new bytes until the terminal `GetJob`
/// was answered), `job run` goes on reading, writes the rest of the stream and then exits
/// with the job's status. Catches a run that leaves at the job's end with the stream cut
/// short. The stream cannot end before the job's end here, so the end is always seen first.
#[test]
fn job_run_writes_the_rest_of_a_stream_after_the_job_has_ended() {
    let scratch = Scratch::new("run_late_stream");
    let fake = Fake::new();
    let ended = Arc::new(AtomicBool::new(false));
    let flag = ended.clone();
    fake.respond("GetJob", move |_| {
        flag.store(true, Ordering::SeqCst);
        job(JobState::Failed)
    });
    fake.answer("GetResult", vec![result(ResultReason::WallLimit)]);
    fake.respond("GetLogs", move |message| {
        let read = GetLogsRequest::decode(message).unwrap();
        let from = read.from_offset.unwrap();
        let live = LogStream::try_from(read.stream).unwrap() == LogStream::Stdout;
        let page = match (live, ended.load(Ordering::SeqCst), from) {
            (true, false, _) => page(0, b"", false),
            (true, true, 0) => page(0, b"late ", false),
            (true, true, _) => page(5, b"bytes", true),
            (false, _, _) => page(0, b"", true),
        };
        Answer::Message(page.encode_to_vec())
    });
    let paces = Arc::new(Paces::default());
    let words = ["job", "run", "--id", "nightly-7", "--spec.image", IMAGE];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (12, "late bytes", "job nightly-7 timed-out\n")
    );
}

/// Asserts that a signal fired while the job runs makes `job run` say so, send exactly one
/// `CancelJob` for the job, go on reading the job after it, and exit with the status of the
/// end the job reached (cancelled, 11) once both streams are at their end. Catches a cancel
/// sent at every pause, a run that exits at the signal without waiting for the end, and a
/// signal that is ignored.
#[test]
fn job_run_cancels_once_at_a_signal_and_waits_for_the_end() {
    let scratch = Scratch::new("run_cancel");
    let fake = Fake::new();
    fake.answer(
        "GetJob",
        vec![
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Cancelled),
        ],
    );
    silent(&fake);
    let paces = Arc::new(Paces::default());
    let signals = Signals::default();
    let firing = signals.clone();
    paces.at(0, move || firing.fire(ForwardedSignal::Interrupt));
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE];
    let got = js(&scratch, &fake, &paces, signals.source(), &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            11,
            "",
            "js: SIGINT: cancelling job j-1 and waiting for it to end; another signal stops the wait\njob j-1 cancelled\n"
        )
    );
    let cancels: Vec<CancelJobRequest> = sent(&fake, "CancelJob");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].job_id, "j-1");
    let order: Vec<String> = calls(&fake)
        .into_iter()
        .filter(|name| name == "GetJob" || name == "CancelJob")
        .collect();
    assert_eq!(order, ["GetJob", "GetJob", "CancelJob", "GetJob", "GetJob"]);
    assert_eq!(paces.asked(), [TWO, TWO, TWO]);
}

/// Asserts that a second signal ends `job run` at once, with 128 plus its number (130 for
/// SIGINT), without another pause and with no second `CancelJob`, saying (here in JSON) that
/// the job may still be running and how to cancel it. Catches a second signal that is
/// ignored or waits for the job's end.
#[test]
fn job_run_stops_waiting_at_a_second_signal() {
    let scratch = Scratch::new("run_twice");
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Running)]);
    silent(&fake);
    let paces = Arc::new(Paces::default());
    let signals = Signals::default();
    for pause in [0, 2] {
        let firing = signals.clone();
        paces.at(pause, move || firing.fire(ForwardedSignal::Interrupt));
    }
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE, "--json"];
    let got = js(&scratch, &fake, &paces, signals.source(), &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            130,
            "",
            "{\"cancelling\":\"j-1\",\"signal\":\"SIGINT\"}\n{\"error\":\"interrupted\",\"message\":\"SIGINT again: stopped waiting; job j-1 may still be running (js job cancel --job-id j-1)\"}\n"
        )
    );
    assert_eq!(sent::<CancelJobRequest>(&fake, "CancelJob").len(), 1);
    assert_eq!(paces.asked(), [TWO, TWO, TWO]);
}

/// Asserts that a refused submit ends `job run` with the refusal's status (3) and reads no
/// job; that a submit whose answer is lost on every try is sent three times and ends with
/// 9 and the same advice as `job submit`; and that a signal source that cannot be installed
/// ends the run before anything is sent. Catches a run that waits on a job that was never
/// made, a submit retried forever, and a run that goes on unable to cancel on a signal.
#[test]
fn job_run_ends_on_a_failed_submit_or_signal_source() {
    let scratch = Scratch::new("run_refused");
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE];
    let fake = Fake::new();
    fake.answer(
        "SubmitJob",
        vec![Answer::Refused(RefusalCode::NoCapableHost, "")],
    );
    let paces = Arc::new(Paces::default());
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.err.as_str()),
        (3, "js: refused: no-capable-host\n")
    );
    assert_eq!(calls(&fake), ["SubmitJob"]);

    let fake = Fake::new();
    fake.answer("SubmitJob", vec![Answer::Status(Code::Unavailable, "lost")]);
    let paces = Arc::new(Paces::default());
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.err.as_str()),
        (
            9,
            "js: the job may exist; run the same command again with --id j-1\n"
        )
    );
    assert_eq!(calls(&fake), ["SubmitJob", "SubmitJob", "SubmitJob"]);
    assert_eq!(paces.asked(), [TWO, TWO]);

    let fake = Fake::new();
    let refusing: SignalSource = Box::new(|_| Err(io::Error::other("no handlers")));
    let got = js(&scratch, &fake, &paces, refusing, &words);
    assert_eq!(
        (got.code, got.err.as_str()),
        (1, "js: cannot watch for signals: no handlers\n")
    );
    assert!(fake.seen().is_empty());
}

/// Asserts that a stream `job run` cannot follow (the server keeps no live logs) is said on
/// stderr, once for each stream, while the run still waits and exits with the job's status.
/// Catches a log failure that changes the job's status or ends the wait.
#[test]
fn job_run_says_a_stream_it_cannot_follow_and_keeps_the_jobs_status() {
    let scratch = Scratch::new("run_nologs");
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    fake.answer(
        "GetLogs",
        vec![Answer::Status(Code::FailedPrecondition, "no-live-logs")],
    );
    let paces = Arc::new(Paces::default());
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!((got.code, got.out.as_str()), (0, ""));
    let mut lines: Vec<&str> = got.err.lines().collect();
    let end = lines.pop();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "js: cannot follow the stderr of job j-1: refused: no-live-logs",
            "js: cannot follow the stdout of job j-1: refused: no-live-logs",
        ]
    );
    assert_eq!(end, Some("job j-1 succeeded"));
}

/// Asserts that a wait that cannot tell the job's end (here a state this `js` does not know)
/// ends `job run` at once with 15 and the reason, though both streams are still open and
/// would only end with the job. Catches a run held until its streams end by a wait that has
/// already failed, with a signal then cancelling nothing.
#[test]
fn job_run_ends_at_once_when_its_wait_fails() {
    let scratch = Scratch::new("run_undetermined");
    let fake = Fake::new();
    let newer = GetJobResponse {
        job: Some(Job {
            state: 99,
            ..Job::default()
        }),
    };
    fake.answer("GetJob", vec![Answer::Message(newer.encode_to_vec())]);
    by_stream(
        &fake,
        [vec![page(0, b"", false)], vec![page(0, b"", false)]],
    );
    let paces = Arc::new(Paces::default());
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (
            15,
            "",
            "js: cannot tell how job j-1 ended: job j-1 is in a state this client does not know (99)\n"
        )
    );
}

/// Asserts that when the job's end is read while both streams are still open, `job run`
/// goes on writing the streams to their end before it prints the end and exits with the
/// job's status. The streams are held open (empty, not at their end) until the job has been
/// read as ended and then for a few more reads, and only after that carry their last bytes
/// and their end, so the wait has ended before either stream can. Catches a run that exits
/// at the job's end and drops what the streams still had to write.
#[test]
fn job_run_writes_the_streams_to_their_end_after_the_job_has_ended() {
    let scratch = Scratch::new("run_drain_after_end");
    let fake = Fake::new();
    let ended = Arc::new(Mutex::new(false));
    let seen_end = ended.clone();
    fake.respond("GetJob", move |_| {
        *seen_end.lock().unwrap() = true;
        job(JobState::Succeeded)
    });
    let after = Arc::new(Mutex::new([0_u32; 2]));
    fake.respond("GetLogs", move |message| {
        let read = GetLogsRequest::decode(message).unwrap();
        let (index, tail): (usize, &[u8]) = match LogStream::try_from(read.stream).unwrap() {
            LogStream::Stdout => (0, b"out tail"),
            _ => (1, b"err tail"),
        };
        let mut after = after.lock().unwrap();
        if *ended.lock().unwrap() {
            after[index] += 1;
        }
        let page = if after[index] > 8 {
            page(0, tail, true)
        } else {
            page(0, b"", false)
        };
        Answer::Message(page.encode_to_vec())
    });
    let paces = Arc::new(Paces::default());
    let words = ["job", "run", "--id", "j-1", "--spec.image", IMAGE];
    let got = quiet(&scratch, &fake, &paces, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "out tail", "err tailjob j-1 succeeded\n")
    );
    let reads: Vec<GetJobRequest> = sent(&fake, "GetJob");
    assert_eq!(reads.len(), 1);
}

/// Asserts that `job wait`, `job follow` and `job run` refuse a token file that others can
/// read before any call, with exit 2 and the same words as a generated job command. Catches
/// a hand-written command that skips the client's token check or names it differently.
#[test]
fn the_hand_written_commands_check_the_token_file_as_the_others_do() {
    let scratch = Scratch::new("hand_token");
    let commands: [&[&str]; 4] = [
        &["job", "get", "--job-id", "j-1"],
        &["job", "wait", "--job-id", "j-1"],
        &["job", "follow", "--job-id", "j-1"],
        &["job", "run", "--id", "j-1", "--spec.image", IMAGE],
    ];
    let mut said = Vec::new();
    for words in commands {
        let fake = Fake::new();
        let services = services()
            .with_dialer(fake.dialer())
            .with_job_waiter(Arc::new(Paces::default()));
        let got = crate::support::run(with_token(&scratch, 0o644, words), services);
        assert_eq!(got.code, 2, "{words:?}: {}", got.err);
        assert!(fake.seen().is_empty(), "{words:?}");
        said.push(got.err);
    }
    assert!(said.iter().all(|err| err == &said[0]), "{said:?}");
}
