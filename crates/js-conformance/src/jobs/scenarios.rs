//! The scenarios of the job-service suite.

use std::time::Duration;

use js_proto::refusal::refusal_of;
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::{
    self, CancelJobRequest, GetJobRequest, GetResultRequest, JobSpec, JobState, ListJobsRequest,
    RefusalCode, ResultReason, SubmitJobRequest,
};
use tonic::transport::Channel;
use tonic::{Code, Response, Status};

use super::{JobHarness, Step, other_spec, spec, submit};
use crate::Verdict;

type Client = JobServiceClient<Channel>;

/// One behaviour every job service must have, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// The same job submitted twice is one job: the second answer is the first, and the
    /// listing holds the id once, also when the resubmit has another priority.
    SubmitIsIdempotent,
    /// The same id with another spec is refused as `ALREADY_EXISTS` with the refusal
    /// `JOB_ID_CONFLICT`, and the recorded job stays as it was.
    SameIdOtherSpecIsRefused,
    /// A job moves queued, placed, starting, running, then succeeded or failed, each move
    /// shows in `GetJob`, the queued, placed, running and ended times follow the moves, and
    /// a nonzero exit code fails the job.
    StatesMoveInOrder,
    /// A move the state machine has no edge for is refused and leaves the job as it was.
    IllegalMovesAreRefused,
    /// A cancel ends a queued job at once, only marks a placed or running one, and leaves
    /// an ended one as it is.
    CancelFollowsTheState,
    /// `GetResult` of a job that has not ended is `FAILED_PRECONDITION` `not-finished`.
    ResultWaitsForTheEnd,
    /// The result of an ended job holds its state, exit code and reason.
    ResultIsTheRecordedOne,
    /// A result reads until its retention bound and is `expired` from it while `GetJob`
    /// still shows the job; the sweep makes the job unknown and frees its id.
    RetentionHoldsUntilItsBound,
    /// Each refusal has its stable gRPC code, message and, where there is one, refusal
    /// code.
    ErrorsHaveStableCodes,
    /// The job records the client that submitted it and shows it on every read.
    ClientIsRecorded,
}

impl Scenario {
    /// Every scenario, in the order [`super::run`] runs them.
    pub const ALL: [Scenario; 10] = [
        Scenario::SubmitIsIdempotent,
        Scenario::SameIdOtherSpecIsRefused,
        Scenario::StatesMoveInOrder,
        Scenario::IllegalMovesAreRefused,
        Scenario::CancelFollowsTheState,
        Scenario::ResultWaitsForTheEnd,
        Scenario::ResultIsTheRecordedOne,
        Scenario::RetentionHoldsUntilItsBound,
        Scenario::ErrorsHaveStableCodes,
        Scenario::ClientIsRecorded,
    ];

    /// Runs this scenario on a fresh harness. Every observation is made, and the verdict
    /// lists each that failed; a call that errs unexpectedly is a failed observation.
    pub async fn run<H: JobHarness>(&self, harness: &mut H) -> Verdict {
        let mut notes = Notes(Vec::new());
        match self {
            Self::SubmitIsIdempotent => submit_is_idempotent(harness, &mut notes).await,
            Self::SameIdOtherSpecIsRefused => same_id_other_spec(harness, &mut notes).await,
            Self::StatesMoveInOrder => states_move_in_order(harness, &mut notes).await,
            Self::IllegalMovesAreRefused => illegal_moves(harness, &mut notes).await,
            Self::CancelFollowsTheState => cancel_follows_the_state(harness, &mut notes).await,
            Self::ResultWaitsForTheEnd => result_waits(harness, &mut notes).await,
            Self::ResultIsTheRecordedOne => result_is_recorded(harness, &mut notes).await,
            Self::RetentionHoldsUntilItsBound => retention(harness, &mut notes).await,
            Self::ErrorsHaveStableCodes => errors_are_stable(harness, &mut notes).await,
            Self::ClientIsRecorded => client_is_recorded(harness, &mut notes).await,
        }
        notes.verdict()
    }
}

/// The observations that did not hold, in the order they were made. A scenario makes
/// every observation, so the verdict names everything that was wrong, not the first.
struct Notes(Vec<String>);

impl Notes {
    fn expect(&mut self, held: bool, what: &str) {
        if !held {
            self.0.push(what.to_string());
        }
    }

    /// The part of the answer `take` picks, or a note that the call failed or that the
    /// answer lacks it.
    fn answer<R, T>(
        &mut self,
        what: &str,
        answer: Result<Response<R>, Status>,
        take: impl FnOnce(R) -> Option<T>,
    ) -> Option<T> {
        let taken = match answer {
            Ok(response) => take(response.into_inner()),
            Err(status) => {
                let (code, message) = (status.code(), status.message());
                self.0
                    .push(format!("{what}: failed with {code:?} {message:?}"));
                return None;
            }
        };
        if taken.is_none() {
            self.0.push(format!("{what}: the answer holds nothing"));
        }
        taken
    }

    /// The status of an answer that must be a refusal with `code` and `message`.
    fn refused<R>(
        &mut self,
        what: &str,
        answer: Result<Response<R>, Status>,
        want: (Code, &str),
    ) -> Option<Status> {
        let Err(status) = answer else {
            self.0
                .push(format!("{what}: was answered, expected {:?}", want.0));
            return None;
        };
        let got = (status.code(), status.message());
        if got != want {
            self.0
                .push(format!("{what}: expected {want:?}, got {got:?}"));
        }
        Some(status)
    }

    fn verdict(self) -> Verdict {
        match self.0.is_empty() {
            true => Verdict::Passed,
            false => Verdict::Failed(self.0.join("; ")),
        }
    }
}

fn id_request(id: &str) -> GetJobRequest {
    GetJobRequest {
        job_id: id.to_string(),
    }
}

async fn submitted(
    client: &mut Client,
    notes: &mut Notes,
    what: &str,
    spec: JobSpec,
) -> Option<v1::Job> {
    let answer = client.submit_job(submit(spec)).await;
    notes.answer(what, answer, |response| response.job)
}

async fn read(client: &mut Client, notes: &mut Notes, id: &str) -> Option<v1::Job> {
    let answer = client.get_job(id_request(id)).await;
    notes.answer(&format!("GetJob {id}"), answer, |response| response.job)
}

async fn cancelled(client: &mut Client, notes: &mut Notes, id: &str) -> Option<v1::Job> {
    let request = CancelJobRequest {
        job_id: id.to_string(),
    };
    let answer = client.cancel_job(request).await;
    notes.answer(&format!("CancelJob {id}"), answer, |response| response.job)
}

async fn result_of(client: &mut Client, notes: &mut Notes, id: &str) -> Option<v1::JobResult> {
    let request = GetResultRequest {
        job_id: id.to_string(),
    };
    let answer = client.get_result(request).await;
    notes.answer(&format!("GetResult {id}"), answer, |response| {
        response.result
    })
}

/// The ids of every job the listing shows.
async fn listed(client: &mut Client, notes: &mut Notes) -> Option<Vec<String>> {
    let answer = client.list_jobs(ListJobsRequest::default()).await;
    let jobs = notes.answer("ListJobs", answer, |response| Some(response.jobs));
    jobs.map(|jobs| jobs.into_iter().map(|job| job.id).collect())
}

fn state_is(job: &Option<v1::Job>, state: JobState) -> bool {
    job.as_ref()
        .is_some_and(|job| job.state == i32::from(state))
}

fn times_of(job: &Option<v1::Job>) -> v1::JobTimes {
    job.as_ref().and_then(|job| job.times).unwrap_or_default()
}

/// Makes `steps` on `id`, noting each one the service refused.
fn drive<H: JobHarness>(harness: &mut H, notes: &mut Notes, id: &str, steps: &[Step]) {
    for step in steps {
        let taken = harness.step(id, *step).is_ok();
        notes.expect(taken, &format!("{id}: {step:?} is taken"));
    }
}

/// The steps that take a queued job to running.
const TO_RUNNING: [Step; 3] = [Step::Place, Step::Start, Step::Run];

/// Submits `id` and makes it run to its end with `code`.
async fn ended<H: JobHarness>(harness: &mut H, notes: &mut Notes, id: &str, code: i32) {
    let mut client = harness.client();
    submitted(&mut client, notes, "SubmitJob", spec(id)).await;
    drive(harness, notes, id, &TO_RUNNING);
    drive(harness, notes, id, &[Step::Exit(code)]);
}

async fn submit_is_idempotent<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    let first = submitted(&mut client, notes, "first SubmitJob", spec("same")).await;
    let again = submitted(&mut client, notes, "second SubmitJob", spec("same")).await;
    notes.expect(
        first == again,
        "a resubmit answers the job already recorded",
    );
    let urgent = JobSpec {
        priority: 9,
        ..spec("same")
    };
    let urgent = submitted(
        &mut client,
        notes,
        "SubmitJob with another priority",
        urgent,
    )
    .await;
    notes.expect(
        first == urgent,
        "a resubmit with another priority answers the recorded job",
    );
    let ids = listed(&mut client, notes).await;
    notes.expect(
        ids == Some(vec!["same".to_string()]),
        "the listing holds the id once",
    );
}

async fn same_id_other_spec<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    let first = submitted(&mut client, notes, "first SubmitJob", spec("taken")).await;
    let answer = client.submit_job(submit(other_spec("taken"))).await;
    let want = (Code::AlreadyExists, "the job id names a different job");
    let status = notes.refused("SubmitJob of another job under the id", answer, want);
    let refusal = status.as_ref().and_then(refusal_of);
    let code = refusal.map(|refusal| refusal.code);
    notes.expect(
        code == Some(i32::from(RefusalCode::JobIdConflict)),
        "the refusal is JOB_ID_CONFLICT",
    );
    let after = read(&mut client, notes, "taken").await;
    notes.expect(
        first == after,
        "the refused submit leaves the recorded job as it was",
    );
}

async fn states_move_in_order<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    let queued = submitted(&mut client, notes, "SubmitJob", spec("walk")).await;
    notes.expect(
        state_is(&queued, JobState::Queued),
        "a submitted job is QUEUED",
    );
    let at = times_of(&queued).queued_at_ms;
    let walk = [
        (Step::Place, JobState::Placed),
        (Step::Start, JobState::Starting),
        (Step::Run, JobState::Running),
        (Step::Exit(0), JobState::Succeeded),
    ];
    for (step, state) in walk {
        harness.advance(Duration::from_secs(1));
        drive(harness, notes, "walk", &[step]);
        let job = read(&mut client, notes, "walk").await;
        notes.expect(
            state_is(&job, state),
            &format!("after {step:?} the job is {state:?}"),
        );
    }
    let job = read(&mut client, notes, "walk").await;
    let t = times_of(&job);
    let seen = (
        t.queued_at_ms,
        t.placed_at_ms,
        t.started_at_ms,
        t.ended_at_ms,
    );
    // A store's clock may move while it commits (the real service's test store charges a
    // millisecond or so per write), so a stamp is held to be at least the advances that
    // came before its move, counted from the stamp of the move before.
    // A stamp a move left out reads as 0, which no later bound allows.
    let [placed, started, finished] = [seen.1, seen.2, seen.3].map(|stamp| stamp.unwrap_or(0));
    let in_order = (seen.0 == at)
        & (placed >= at.saturating_add(1_000))
        & (started >= placed.saturating_add(2_000))
        & (finished >= started.saturating_add(1_000));
    notes.expect(in_order, "the job's times are those of its moves");
    ended(harness, notes, "bad", 7).await;
    let bad = read(&mut client, notes, "bad").await;
    notes.expect(
        state_is(&bad, JobState::Failed),
        "a nonzero exit code fails the job",
    );
}

/// A job's life up to a state, and the steps with no edge from it.
struct Illegal {
    /// The state the job is brought to.
    at: &'static [Step],
    /// The steps no edge leaves it by.
    refused: &'static [Step],
}

const ILLEGAL: [Illegal; 5] = [
    Illegal {
        at: &[],
        refused: &[Step::Start, Step::Run, Step::Exit(0)],
    },
    Illegal {
        at: &[Step::Place],
        refused: &[Step::Place, Step::Run, Step::Exit(0)],
    },
    Illegal {
        at: &[Step::Place, Step::Start],
        refused: &[Step::Place, Step::Start, Step::Exit(0)],
    },
    Illegal {
        at: &TO_RUNNING,
        refused: &[Step::Place, Step::Start, Step::Run],
    },
    Illegal {
        at: &[Step::Place, Step::Start, Step::Run, Step::Exit(0)],
        refused: &[
            Step::Place,
            Step::Start,
            Step::Run,
            Step::Exit(0),
            Step::Exit(1),
        ],
    },
];

async fn illegal_moves<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    for (n, case) in ILLEGAL.iter().enumerate() {
        let id = format!("illegal-{n}");
        submitted(&mut client, notes, "SubmitJob", spec(&id)).await;
        drive(harness, notes, &id, case.at);
        let before = read(&mut client, notes, &id).await;
        for step in case.refused {
            let refused = harness.step(&id, *step) == Err(super::Refused);
            notes.expect(refused, &format!("{id}: {step:?} is refused"));
        }
        let after = read(&mut client, notes, &id).await;
        notes.expect(
            before == after,
            &format!("{id}: refused moves leave the job as it was"),
        );
    }
    submitted(&mut client, notes, "SubmitJob", spec("cancelled")).await;
    cancelled(&mut client, notes, "cancelled").await;
    let before = read(&mut client, notes, "cancelled").await;
    let refused = harness.step("cancelled", Step::Place) == Err(super::Refused);
    notes.expect(refused, "a cancelled job is not placed");
    let after = read(&mut client, notes, "cancelled").await;
    notes.expect(
        before == after,
        "the refused placement leaves the cancelled job as it was",
    );
    let unknown = harness.step("nobody", Step::Place) == Err(super::Refused);
    notes.expect(unknown, "a step on an id no job has is refused");
}

async fn cancel_follows_the_state<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    submitted(&mut client, notes, "SubmitJob", spec("queued")).await;
    let job = cancelled(&mut client, notes, "queued").await;
    notes.expect(
        state_is(&job, JobState::Cancelled),
        "a queued job is cancelled at once",
    );
    let result = result_of(&mut client, notes, "queued").await;
    let seen = result.map(|result| (result.state, result.reason, result.exit_code));
    let want = (
        JobState::Cancelled.into(),
        ResultReason::Cancelled.into(),
        None,
    );
    notes.expect(
        seen == Some(want),
        "a cancelled job's result is CANCELLED with reason CANCELLED",
    );

    submitted(&mut client, notes, "SubmitJob", spec("placed")).await;
    drive(harness, notes, "placed", &[Step::Place]);
    let job = cancelled(&mut client, notes, "placed").await;
    let held = state_is(&job, JobState::Placed) && job.is_some_and(|job| job.cancel_requested);
    notes.expect(
        held,
        "a cancel of a placed job marks cancel_requested and keeps PLACED",
    );

    submitted(&mut client, notes, "SubmitJob", spec("running")).await;
    drive(harness, notes, "running", &TO_RUNNING);
    let job = cancelled(&mut client, notes, "running").await;
    let marked = job.as_ref().is_some_and(|job| job.cancel_requested);
    notes.expect(
        state_is(&job, JobState::Running) && marked,
        "a cancel of a running job marks cancel_requested and keeps RUNNING",
    );
    let again = cancelled(&mut client, notes, "running").await;
    notes.expect(
        job == again,
        "a second cancel answers the job as the first left it",
    );

    ended(harness, notes, "done", 0).await;
    let before = read(&mut client, notes, "done").await;
    let job = cancelled(&mut client, notes, "done").await;
    notes.expect(
        before == job,
        "a cancel of an ended job leaves it as it was",
    );
    notes.expect(
        state_is(&job, JobState::Succeeded),
        "an ended job stays SUCCEEDED",
    );
}

async fn result_waits<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    let want = (Code::FailedPrecondition, "not-finished");
    submitted(&mut client, notes, "SubmitJob", spec("waiting")).await;
    let request = GetResultRequest {
        job_id: "waiting".to_string(),
    };
    let answer = client.get_result(request.clone()).await;
    notes.refused("GetResult of a queued job", answer, want);
    submitted(&mut client, notes, "SubmitJob", spec("busy")).await;
    drive(harness, notes, "busy", &TO_RUNNING);
    let request = GetResultRequest {
        job_id: "busy".to_string(),
    };
    let answer = client.get_result(request).await;
    notes.refused("GetResult of a running job", answer, want);
}

async fn result_is_recorded<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    for (id, code, state) in [
        ("zero", 0, JobState::Succeeded),
        ("three", 3, JobState::Failed),
    ] {
        ended(harness, notes, id, code).await;
        let result = result_of(&mut client, notes, id).await;
        let seen = result
            .as_ref()
            .map(|result| (result.state, result.exit_code, result.reason));
        let want = (state.into(), Some(code), ResultReason::Exit.into());
        notes.expect(
            seen == Some(want),
            &format!("{id}: the result holds the state, exit code {code} and reason EXIT"),
        );
        let job = read(&mut client, notes, id).await;
        let ended_at = result
            .and_then(|result| result.times)
            .and_then(|times| times.ended_at_ms);
        notes.expect(
            ended_at.is_some() && ended_at == times_of(&job).ended_at_ms,
            &format!("{id}: the result's end time is the job's"),
        );
    }
}

async fn retention<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    submitted(&mut client, notes, "SubmitJob", spec("waiting")).await;
    ended(harness, notes, "old", 0).await;
    let window = harness.retention();
    harness.advance(window.saturating_sub(Duration::from_secs(1)));
    result_of(&mut client, notes, "old").await;
    harness.advance(Duration::from_secs(1));
    let request = GetResultRequest {
        job_id: "old".to_string(),
    };
    let answer = client.get_result(request).await;
    notes.refused(
        "GetResult at the retention bound",
        answer,
        (Code::NotFound, "expired"),
    );
    let job = read(&mut client, notes, "old").await;
    notes.expect(
        state_is(&job, JobState::Succeeded),
        "the job is shown until the sweep",
    );
    harness.sweep();
    let gone = (Code::NotFound, "unknown-job");
    let answer = client.get_job(id_request("old")).await;
    notes.refused("GetJob after the sweep", answer, gone);
    let request = GetResultRequest {
        job_id: "old".to_string(),
    };
    let answer = client.get_result(request).await;
    notes.refused("GetResult after the sweep", answer, gone);
    let request = CancelJobRequest {
        job_id: "old".to_string(),
    };
    let answer = client.cancel_job(request).await;
    notes.refused("CancelJob after the sweep", answer, gone);
    let waiting = read(&mut client, notes, "waiting").await;
    notes.expect(
        state_is(&waiting, JobState::Queued),
        "the sweep keeps a job that has not ended",
    );
    let again = submitted(&mut client, notes, "SubmitJob of the swept id", spec("old")).await;
    notes.expect(
        state_is(&again, JobState::Queued),
        "a swept id is free: the new job is QUEUED",
    );
}

async fn errors_are_stable<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    let gone = (Code::NotFound, "unknown-job");
    let malformed = (Code::InvalidArgument, "field: jobId");
    for (id, want) in [("nobody", gone), ("bad id!", malformed)] {
        let answer = client.get_job(id_request(id)).await;
        notes.refused(&format!("GetJob {id:?}"), answer, want);
        let request = GetResultRequest {
            job_id: id.to_string(),
        };
        let answer = client.get_result(request).await;
        notes.refused(&format!("GetResult {id:?}"), answer, want);
        let request = CancelJobRequest {
            job_id: id.to_string(),
        };
        let answer = client.cancel_job(request).await;
        notes.refused(&format!("CancelJob {id:?}"), answer, want);
    }
    let answer = client.submit_job(SubmitJobRequest { spec: None }).await;
    notes.refused(
        "SubmitJob without a spec",
        answer,
        (Code::InvalidArgument, "field: spec"),
    );
    let answer = client.submit_job(submit(spec("bad id!"))).await;
    notes.refused(
        "SubmitJob with a malformed id",
        answer,
        (Code::InvalidArgument, "field: spec.jobId"),
    );
    for (what, wall) in [("0", Some(0)), ("none", None)] {
        let limited = JobSpec {
            wall_limit_seconds: wall,
            ..spec("limited")
        };
        let answer = client.submit_job(submit(limited)).await;
        let status = answer.err();
        let refusal = status.as_ref().and_then(refusal_of);
        let seen = (
            status.map(|status| status.code()),
            refusal.map(|refusal| refusal.code),
        );
        let want = (
            Some(Code::InvalidArgument),
            Some(i32::from(RefusalCode::WallTimeNotPositive)),
        );
        notes.expect(
            seen == want,
            &format!("a wall limit of {what} is refused as WALL_TIME_NOT_POSITIVE"),
        );
    }
    let ids = listed(&mut client, notes).await;
    notes.expect(ids == Some(Vec::new()), "a refused submit records nothing");
}

async fn client_is_recorded<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let name = Some(harness.client_name());
    let mut client = harness.client();
    let job = submitted(&mut client, notes, "SubmitJob", spec("mine")).await;
    notes.expect(
        job.and_then(|job| job.client) == name,
        "the submit answer names the client",
    );
    let job = read(&mut client, notes, "mine").await;
    notes.expect(
        job.and_then(|job| job.client) == name,
        "GetJob names the client",
    );
    let answer = client.list_jobs(ListJobsRequest::default()).await;
    let jobs = notes.answer("ListJobs", answer, |response| Some(response.jobs));
    let first = jobs.and_then(|jobs| jobs.into_iter().next());
    notes.expect(
        first.and_then(|job| job.client) == name,
        "ListJobs names the client",
    );
}
