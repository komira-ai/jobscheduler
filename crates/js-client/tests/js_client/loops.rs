//! The loops of `js_client::jobs` (`wait_for_terminal`, `follow_logs`, `submit_retrying`)
//! against a fake of the calls they make, scripted with the answers of each call in order,
//! and a waiter that records each pause asked of it and returns at once: nothing here waits
//! for a clock. And the calls through `JobCalls` on a real `JobClient`, and the tokio
//! waiter on a paused clock.

use std::collections::VecDeque;
use std::future::{pending, ready};
use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Waker};
use std::time::Duration;

use js_client::jobs::{
    FollowError, JobCalls, JobClient, JobError, NotSent, Outcome, Piece, TokioWaiter, WaitError,
    Waiter, follow_logs, submit_retrying, wait_for_terminal,
};
use js_proto::v1::{
    CancelJobRequest, CancelJobResponse, GetJobRequest, GetJobResponse, GetLogsRequest,
    GetLogsResponse, GetResultRequest, GetResultResponse, Job, JobResult, JobSpec, JobState,
    LogGap, LogStream, ResultReason, SubmitJobRequest, SubmitJobResponse,
};
use prost::Message as _;
use tonic::Code;

use crate::fake_jobs::{Answer, Fake};
use crate::support::Scratch;

/// The answers of a call, in order. A call with none left fails the test.
type Answers<T> = VecDeque<Result<T, JobError>>;

/// The fake: the answers of each call, and every call it was asked, as `Call job-id` (and
/// the offset of a `GetLogs`).
#[derive(Default)]
struct Script {
    submits: Answers<SubmitJobResponse>,
    jobs: Answers<GetJobResponse>,
    cancels: Answers<CancelJobResponse>,
    results: Answers<GetResultResponse>,
    logs: Answers<GetLogsResponse>,
    calls: Vec<String>,
    log_requests: Vec<GetLogsRequest>,
    submitted: Vec<SubmitJobRequest>,
}

/// The next answer of a call, or the test fails naming the call.
fn next<T>(answers: &mut Answers<T>, call: &str) -> Result<T, JobError> {
    answers
        .pop_front()
        .unwrap_or_else(|| panic!("{call} was called with no answer left"))
}

impl JobCalls for Script {
    async fn submit_job(
        &mut self,
        request: SubmitJobRequest,
    ) -> Result<SubmitJobResponse, JobError> {
        self.calls.push("SubmitJob".to_owned());
        self.submitted.push(request);
        next(&mut self.submits, "SubmitJob")
    }

    async fn get_job(&mut self, request: GetJobRequest) -> Result<GetJobResponse, JobError> {
        self.calls.push(format!("GetJob {}", request.job_id));
        next(&mut self.jobs, "GetJob")
    }

    async fn cancel_job(
        &mut self,
        request: CancelJobRequest,
    ) -> Result<CancelJobResponse, JobError> {
        self.calls.push(format!("CancelJob {}", request.job_id));
        next(&mut self.cancels, "CancelJob")
    }

    async fn get_result(
        &mut self,
        request: GetResultRequest,
    ) -> Result<GetResultResponse, JobError> {
        self.calls.push(format!("GetResult {}", request.job_id));
        next(&mut self.results, "GetResult")
    }

    async fn get_logs(&mut self, request: GetLogsRequest) -> Result<GetLogsResponse, JobError> {
        self.calls.push(format!(
            "GetLogs {} {}",
            request.job_id,
            request.from_offset.unwrap_or_default()
        ));
        self.log_requests.push(request);
        next(&mut self.logs, "GetLogs")
    }
}

/// A waiter that records every pause asked of it and returns at once.
#[derive(Default)]
struct Paces(Mutex<Vec<Duration>>);

impl Paces {
    fn asked(&self) -> Vec<Duration> {
        self.0.lock().unwrap().clone()
    }
}

impl Waiter for Paces {
    fn wait(&self, interval: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.0.lock().unwrap().push(interval);
        Box::pin(ready(()))
    }
}

/// A read of a job in `state`.
fn job(state: JobState) -> Result<GetJobResponse, JobError> {
    Ok(GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            state: state.into(),
            ..Job::default()
        }),
    })
}

/// A result whose reason is `reason`.
fn result(reason: ResultReason) -> Result<GetResultResponse, JobError> {
    Ok(GetResultResponse {
        result: Some(JobResult {
            state: JobState::Failed.into(),
            reason: reason.into(),
            ..JobResult::default()
        }),
    })
}

/// A page of a stream: `data` from `offset`, ending the stream when `eof`.
fn page(offset: u64, data: &[u8], eof: bool) -> Result<GetLogsResponse, JobError> {
    Ok(GetLogsResponse {
        state: JobState::Running.into(),
        data: data.to_vec(),
        offset,
        next_offset: offset + u64::try_from(data.len()).unwrap(),
        eof,
        ..GetLogsResponse::default()
    })
}

/// An answer that was lost after the request was sent.
fn lost() -> JobError {
    JobError::MayHaveActed("no answer".to_owned())
}

/// Two seconds, the pause between two reads of a live job and two tries of a submit.
const TWO: Duration = Duration::from_secs(2);

/// Asserts that a job read as running, running, then succeeded is `Succeeded` after two
/// pauses of two seconds, one after each live read, and that every read names the job.
/// Catches a wait that ends at the first read, or polls without asking the waiter.
#[tokio::test]
async fn a_wait_pauses_after_each_live_read_until_the_job_ends() {
    let mut fake = Script {
        jobs: [
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Succeeded),
        ]
        .into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let got = wait_for_terminal(&mut fake, &paces, "j-1", pending()).await;
    assert_eq!(got.unwrap(), Outcome::Succeeded);
    assert_eq!(paces.asked(), [TWO, TWO]);
    assert_eq!(fake.calls, ["GetJob j-1", "GetJob j-1", "GetJob j-1"]);
}

/// Asserts the outcome of every ended state, each of the live states read as live first,
/// and that only a failed job's result is read: wall limit is `TimedOut`, memory and an exit
/// are `Failed`, and a failed job with no result in the answer is `Failed`. Catches a state
/// read as another's outcome, and a timed-out job reported as merely failed.
#[tokio::test]
async fn every_ended_state_has_its_own_outcome() {
    let live = [
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
    ];
    let cases = [
        (JobState::Succeeded, None, Outcome::Succeeded),
        (JobState::Cancelled, None, Outcome::Cancelled),
        (JobState::Lost, None, Outcome::Lost),
        (JobState::FailedToStart, None, Outcome::FailedToStart),
        (
            JobState::Failed,
            Some(result(ResultReason::WallLimit)),
            Outcome::TimedOut,
        ),
        (
            JobState::Failed,
            Some(result(ResultReason::Memory)),
            Outcome::Failed,
        ),
        (
            JobState::Failed,
            Some(result(ResultReason::Exit)),
            Outcome::Failed,
        ),
        (
            JobState::Failed,
            Some(Ok(GetResultResponse::default())),
            Outcome::Failed,
        ),
    ];
    for (state, answer, want) in cases {
        let mut jobs: Answers<GetJobResponse> = live.iter().map(|state| job(*state)).collect();
        jobs.push_back(job(state));
        let mut fake = Script {
            jobs,
            results: answer.into_iter().collect(),
            ..Script::default()
        };
        let paces = Paces::default();
        let got = wait_for_terminal(&mut fake, &paces, "j-1", pending()).await;
        assert_eq!(got.unwrap(), want, "{state:?}");
        assert_eq!(paces.asked().len(), live.len(), "{state:?}");
        let read_result = fake.calls.last().unwrap() == "GetResult j-1";
        assert_eq!(read_result, state == JobState::Failed, "{state:?}");
    }
}

/// Asserts that a failed read, mid-wait or of a failed job's result, ends the wait with the
/// call's error and no outcome, and that a state this client does not know (a newer
/// server's, or none at all) is `UnknownState` with the whole message. Catches an error or
/// an unknown state reported as a job's end, and a wait that polls an unknown state forever.
#[tokio::test]
async fn a_failed_read_or_an_unknown_state_is_no_outcome() {
    let paces = Paces::default();
    let mut fake = Script {
        jobs: [job(JobState::Running), Err(lost())].into(),
        ..Script::default()
    };
    let got = wait_for_terminal(&mut fake, &paces, "j-1", pending()).await;
    assert!(matches!(
        got,
        Err(WaitError::Call(JobError::MayHaveActed(_)))
    ));

    let mut fake = Script {
        jobs: [job(JobState::Failed)].into(),
        results: [Err(JobError::Rejected {
            code: Code::NotFound,
            reason: "expired".to_owned(),
        })]
        .into(),
        ..Script::default()
    };
    let got = wait_for_terminal(&mut fake, &paces, "j-1", pending()).await;
    assert_eq!(got.unwrap_err().to_string(), "expired (not-found)");

    let newer = Ok(GetJobResponse {
        job: Some(Job {
            state: 99,
            ..Job::default()
        }),
    });
    for (answer, state) in [(newer, 99), (Ok(GetJobResponse::default()), 0)] {
        let mut fake = Script {
            jobs: [answer].into(),
            ..Script::default()
        };
        let got = wait_for_terminal(&mut fake, &paces, "j-1", pending()).await;
        assert_eq!(
            got.unwrap_err().to_string(),
            format!("job j-1 is in a state this client does not know ({state})")
        );
    }
}

/// Asserts that a cancel that is due at the first pause sends one `CancelJob` for the job,
/// that the wait then goes on reading (at once, then with its pauses) until the job has
/// ended, and that the outcome is the job's real end: cancelled, or succeeded when it got
/// there first. Catches a cancel sent at every pause, a wait that stops at the cancel
/// without reading the end, and a cancel reported as cancelled whatever the job did.
#[tokio::test]
async fn a_due_cancel_is_sent_once_and_the_wait_reads_the_real_end() {
    let mut fake = Script {
        jobs: [
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Running),
            job(JobState::Cancelled),
        ]
        .into(),
        cancels: [Ok(CancelJobResponse::default())].into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let got = wait_for_terminal(&mut fake, &paces, "j-1", ready(())).await;
    assert_eq!(got.unwrap(), Outcome::Cancelled);
    assert_eq!(
        fake.calls,
        [
            "GetJob j-1",
            "CancelJob j-1",
            "GetJob j-1",
            "GetJob j-1",
            "GetJob j-1"
        ]
    );
    assert_eq!(paces.asked(), [TWO, TWO, TWO]);

    let mut fake = Script {
        jobs: [job(JobState::Running), job(JobState::Succeeded)].into(),
        cancels: [Ok(CancelJobResponse::default())].into(),
        ..Script::default()
    };
    let got = wait_for_terminal(&mut fake, &paces, "j-1", ready(())).await;
    assert_eq!(got.unwrap(), Outcome::Succeeded);

    let mut fake = Script {
        jobs: [job(JobState::Running)].into(),
        cancels: [Err(lost())].into(),
        ..Script::default()
    };
    let got = wait_for_terminal(&mut fake, &paces, "j-1", ready(())).await;
    assert!(matches!(
        got,
        Err(WaitError::Call(JobError::MayHaveActed(_)))
    ));
}

/// Asserts that the stream's pages, across calls whose offsets advance by each answer's
/// `next_offset`, reach the sink as their concatenation exactly once; that a page that says
/// the job ended but is not `eof` is read past (the drain); that an empty page is followed by
/// a one-second pause and the same offset; and that every read asks to follow the named
/// stream. Catches bytes read twice or skipped at a page boundary, and a follow that stops at
/// the first sight of the end with bytes unread.
#[tokio::test]
async fn a_follow_hands_on_each_byte_once_and_ends_only_at_eof() {
    let mut ended = page(5, b" ", false).unwrap();
    ended.state = JobState::Succeeded.into();
    let mut fake = Script {
        logs: [
            page(0, b"hel", false),
            page(3, b"lo", false),
            page(5, b"", false),
            Ok(ended),
            page(6, b"world", true),
        ]
        .into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let mut out = Vec::new();
    let got = follow_logs(&mut fake, &paces, "j-1", LogStream::Stdout, |piece| {
        if let Piece::Data(bytes) = piece {
            out.extend_from_slice(bytes);
        }
        Ok(())
    })
    .await;
    got.unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "hello world");
    let offsets: Vec<u64> = fake
        .log_requests
        .iter()
        .map(|request| request.from_offset.unwrap())
        .collect();
    assert_eq!(offsets, [0, 3, 5, 5, 6]);
    assert!(fake.log_requests.iter().all(|request| {
        request.follow == Some(true)
            && request.stream == i32::from(LogStream::Stdout)
            && request.job_id == "j-1"
            && request.max_bytes.is_none()
    }));
    assert_eq!(paces.asked(), [Duration::from_secs(1)]);
}

/// Asserts that a follow refused because too many wait is tried again from the same offset
/// after a five-second pause, that the gaps of a page reach the sink in order before its
/// data, and that the stream named is the one read. Catches a busy refusal that ends the
/// follow, and a gap dropped without a word.
#[tokio::test]
async fn a_busy_follow_is_tried_again_and_gaps_are_handed_on() {
    let mut gapped = page(10, b"tail", true).unwrap();
    gapped.gaps = vec![
        LogGap {
            offset: 0,
            length: 4,
        },
        LogGap {
            offset: 6,
            length: 4,
        },
    ];
    let mut fake = Script {
        logs: [
            Err(JobError::Rejected {
                code: Code::ResourceExhausted,
                reason: "too-many-follows".to_owned(),
            }),
            Ok(gapped),
        ]
        .into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let mut pieces = Vec::new();
    follow_logs(&mut fake, &paces, "j-1", LogStream::Stderr, |piece| {
        pieces.push(match piece {
            Piece::Data(bytes) => format!("data {}", String::from_utf8_lossy(bytes)),
            Piece::Gap { offset, length } => format!("gap {offset}+{length}"),
        });
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(pieces, ["gap 0+4", "gap 6+4", "data tail"]);
    assert_eq!(fake.calls, ["GetLogs j-1 0", "GetLogs j-1 0"]);
    assert_eq!(fake.log_requests[0].stream, i32::from(LogStream::Stderr));
    assert_eq!(paces.asked(), [Duration::from_secs(5)]);
}

/// Asserts that a refusal other than a busy one ends the follow with the call's error, and
/// that a sink that fails on data, or only on a gap (taking the data), ends it with `Write`
/// and the sink's reason.
/// Catches a follow that retries every error forever, and one that drops bytes its sink
/// could not take.
#[tokio::test]
async fn a_refused_follow_or_a_failing_sink_ends_the_follow() {
    let paces = Paces::default();
    let mut fake = Script {
        logs: [Err(JobError::Rejected {
            code: Code::FailedPrecondition,
            reason: "no-live-logs".to_owned(),
        })]
        .into(),
        ..Script::default()
    };
    let got = follow_logs(&mut fake, &paces, "j-1", LogStream::Stdout, |_| Ok(())).await;
    assert_eq!(
        got.unwrap_err().to_string(),
        "no-live-logs (failed-precondition)"
    );

    let mut gapped = page(4, b"", true).unwrap();
    gapped.gaps = vec![LogGap {
        offset: 0,
        length: 4,
    }];
    // The second case's sink takes the data and refuses the gap alone.
    for (answer, gaps_only) in [(page(0, b"abc", true), false), (Ok(gapped), true)] {
        let mut fake = Script {
            logs: [answer].into(),
            ..Script::default()
        };
        let got = follow_logs(&mut fake, &paces, "j-1", LogStream::Stdout, |piece| match (
            gaps_only, piece,
        ) {
            (true, Piece::Data(_)) => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
        })
        .await;
        let error = got.unwrap_err();
        assert!(matches!(error, FollowError::Write(_)));
        assert_eq!(error.to_string(), "cannot write the log: closed");
    }
}

/// Asserts that a follow whose answer is lost, or whose connection failed, is sent again
/// from the same offset after a two-second pause; that the count of such tries starts again
/// after an answer; and that the third loss in a row ends the follow with the call's error.
/// Catches a transient store failure that cuts a stream short, a retry from another offset
/// (bytes twice or dropped), and a follow retried forever against a server that is gone.
#[tokio::test]
async fn a_lost_follow_is_sent_again_three_times_in_a_row_at_most() {
    let unreached = || JobError::NotSent(NotSent::Unreached("refused".to_owned()));
    let mut fake = Script {
        logs: [
            Err(lost()),
            Err(unreached()),
            page(0, b"ab", false),
            Err(lost()),
            Err(lost()),
            page(2, b"c", true),
        ]
        .into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let mut out = Vec::new();
    follow_logs(&mut fake, &paces, "j-1", LogStream::Stdout, |piece| {
        if let Piece::Data(bytes) = piece {
            out.extend_from_slice(bytes);
        }
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(out, b"abc");
    let offsets: Vec<u64> = fake
        .log_requests
        .iter()
        .map(|request| request.from_offset.unwrap())
        .collect();
    assert_eq!(offsets, [0, 0, 0, 2, 2, 2]);
    assert_eq!(paces.asked(), [TWO; 4]);

    let mut fake = Script {
        logs: [Err(lost()), Err(lost()), Err(lost())].into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let got = follow_logs(&mut fake, &paces, "j-1", LogStream::Stdout, |_| Ok(())).await;
    assert!(matches!(
        got,
        Err(FollowError::Call(JobError::MayHaveActed(_)))
    ));
    assert_eq!(fake.log_requests.len(), 3);
    assert_eq!(paces.asked(), [TWO, TWO]);
}

/// A submit of a job with id `id`.
fn submit(id: &str) -> SubmitJobRequest {
    SubmitJobRequest {
        spec: Some(JobSpec {
            job_id: id.to_owned(),
            image: "registry.example/app:1".to_owned(),
            ..JobSpec::default()
        }),
    }
}

/// Asserts that a submit with an id whose answer was lost, then whose connection failed, is
/// sent again with the same request each time, with a two-second pause between, and returns
/// the answer of the try that got one. Catches a retry that changes the request (a new id
/// would make a second job) and a retry without a pause.
#[tokio::test]
async fn a_named_submit_is_sent_again_unchanged_while_its_answer_is_lost() {
    let mut fake = Script {
        submits: [
            Err(lost()),
            Err(JobError::NotSent(NotSent::Unreached("refused".to_owned()))),
            Ok(SubmitJobResponse::default()),
        ]
        .into(),
        ..Script::default()
    };
    let paces = Paces::default();
    let got = submit_retrying(&mut fake, &paces, submit("nightly-1")).await;
    assert!(got.is_ok());
    assert_eq!(fake.submitted, vec![submit("nightly-1"); 3]);
    assert_eq!(paces.asked(), [TWO, TWO]);
}

/// Asserts that a named submit is sent three times at most and then returns the last
/// failure; that a refusal or a refused token is returned at once; and that a submit without
/// an id is sent once, lost answer or not. Catches a submit retried forever, a refusal
/// retried, and an id-less retry that could make a second job.
#[tokio::test]
async fn a_submit_is_not_retried_past_three_tries_a_refusal_or_without_an_id() {
    let paces = Paces::default();
    let mut fake = Script {
        submits: [Err(lost()), Err(lost()), Err(lost())].into(),
        ..Script::default()
    };
    let got = submit_retrying(&mut fake, &paces, submit("nightly-1")).await;
    assert!(got.unwrap_err().may_have_acted());
    assert_eq!(fake.submitted.len(), 3);
    assert_eq!(paces.asked(), [TWO, TWO]);

    let refusals = [
        JobError::Rejected {
            code: Code::FailedPrecondition,
            reason: "no-capable-host".to_owned(),
        },
        JobError::NotSent(NotSent::Token("UNAUTHENTICATED".to_owned())),
    ];
    for refusal in refusals {
        let mut fake = Script {
            submits: [Err(refusal)].into(),
            ..Script::default()
        };
        assert!(
            submit_retrying(&mut fake, &paces, submit("nightly-1"))
                .await
                .is_err()
        );
        assert_eq!(fake.submitted.len(), 1);
    }

    for request in [submit(""), SubmitJobRequest::default()] {
        let mut fake = Script {
            submits: [Err(lost())].into(),
            ..Script::default()
        };
        assert!(submit_retrying(&mut fake, &paces, request).await.is_err());
        assert_eq!(fake.submitted.len(), 1);
    }
    assert_eq!(paces.asked().len(), 2);
}

/// Asserts that the tokio waiter's pause is not over until tokio's clock (paused here, and
/// moved only by the test) has moved by its whole interval, and is over once it has.
/// Catches a waiter that returns before its interval, or ignores it.
#[tokio::test(start_paused = true)]
async fn the_tokio_waiter_completes_after_its_interval() {
    let mut cx = Context::from_waker(Waker::noop());
    let mut pause = TokioWaiter.wait(Duration::from_secs(3600));
    assert!(pause.as_mut().poll(&mut cx).is_pending());
    tokio::time::advance(Duration::from_secs(3599)).await;
    assert!(pause.as_mut().poll(&mut cx).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(pause.as_mut().poll(&mut cx).is_ready());
}

/// Asserts that each call of `JobCalls` on a real `JobClient` is the call of its name on
/// the wire, with the request's bytes and the answer decoded. Catches a trait method wired
/// to another call.
#[tokio::test]
async fn job_calls_on_a_job_client_are_its_own_calls() {
    let dir = Scratch::new("loops_calls");
    let token = dir.write("token", b"job-token-0123456789\n", 0o600);
    let fake = Fake::new();
    let ended = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            ..Job::default()
        }),
    };
    fake.answer("GetJob", vec![Answer::Message(ended.encode_to_vec())]);
    let mut client = JobClient::connect_via(
        "http://localhost:7070",
        &token,
        None,
        Duration::from_secs(20),
        fake.dialer(None),
    )
    .await
    .unwrap();
    let id = || "j-1".to_owned();
    let read = JobCalls::get_job(&mut client, GetJobRequest { job_id: id() }).await;
    assert_eq!(read.unwrap(), ended);
    JobCalls::submit_job(&mut client, submit("j-1"))
        .await
        .unwrap();
    JobCalls::cancel_job(&mut client, CancelJobRequest { job_id: id() })
        .await
        .unwrap();
    JobCalls::get_result(&mut client, GetResultRequest { job_id: id() })
        .await
        .unwrap();
    let logs = GetLogsRequest {
        job_id: id(),
        ..GetLogsRequest::default()
    };
    JobCalls::get_logs(&mut client, logs.clone()).await.unwrap();
    let seen: Vec<(String, Vec<u8>)> = fake
        .seen()
        .into_iter()
        .map(|seen| (seen.path, seen.message))
        .collect();
    let path = |rpc: &str| format!("/jobscheduler.v1.JobService/{rpc}");
    let get = GetJobRequest { job_id: id() }.encode_to_vec();
    assert_eq!(
        seen,
        [
            (path("GetJob"), get.clone()),
            (path("SubmitJob"), submit("j-1").encode_to_vec()),
            (path("CancelJob"), get.clone()),
            (path("GetResult"), get),
            (path("GetLogs"), logs.encode_to_vec()),
        ]
    );
}
