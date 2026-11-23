//! One clock for every shared time: the job pass, its duty claims and the job service's
//! retention check judge the deadlines the store stamped by the store's clock, never by the
//! server's, and decide nothing when the store cannot read its clock. The server's clock and
//! the store's are two manual clocks here ([`two_clock_rig`]), set apart by the test; nothing
//! sleeps.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobSpec, JobState, NewJob, Output, StartFailure};
use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{GetResultRequest, JobResult};
use js_state::StoreError;
use js_types::{ManualClock, Timestamp};
use tonic::{Code, Request, Status};

use crate::agent_api::job_rig::{Rig, spec, two_clock_rig};
use crate::job_api::JobApi;
use crate::job_api::roles::as_operator;
use crate::job_expiry::JobLimits;
use crate::store_health::{FailureKind, Pass};

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// A queue wait of half an hour, and no start timeout a test reaches.
const LIMITS: JobLimits = JobLimits {
    max_queue_wait: Duration::from_secs(30 * 60),
    start_timeout: Duration::from_secs(365 * 24 * 3600),
    ..JobLimits::DEFAULT
};

/// The error a stopped database answers.
fn stopped() -> StoreError {
    StoreError::Disconnected("the database is stopped".to_owned())
}

/// Moves the server's `clock` on by `by`.
fn advance_server(clock: &ManualClock, by: Duration) {
    let now = js_types::Clock::now(clock).millis();
    let by = u64::try_from(by.as_millis()).unwrap();
    clock.set(Timestamp::from_millis(now + by));
}

/// Records the queued job `id` with `spec`.
fn queue(rig: &Rig, id: &str, spec: JobSpec) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// Whether a pass ran (`Some(true)`), was left to another server (`None`) or failed.
fn ran(pass: Option<Pass>) -> Option<bool> {
    pass.map(|pass| matches!(pass, Pass::Ran(_)))
}

// The queue wait is judged by the store's clock, which stamped the submit: with the server's
// clock a day ahead of the store's, a pass ends nothing, and once the store's clock passes the
// wait, a pass ends the job though the server's clock has not moved again. It catches a pass
// that reads the server's clock for the store's (a server whose clock runs ahead fails every
// queued job of the store early, and one whose clock lags keeps them past their wait).
#[test]
fn the_pass_judges_a_queue_wait_by_the_store_clock_not_the_servers() {
    let (rig, server) = two_clock_rig("clock_queue_wait", LIMITS);
    let id = queue(&rig, "job-q", spec());
    advance_server(&server, DAY);
    assert_eq!(ran(rig.agents.job_pass()), Some(true));
    assert_eq!(rig.job(&id).state, JobState::Queued);

    rig.advance(LIMITS.max_queue_wait);
    assert_eq!(ran(rig.agents.job_pass()), Some(true));
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
}

// A duty's period is the store's clock: a pass claimed when the store's clock reads its start
// is not claimed again after the server's clock moved an hour, while the store's has not moved,
// and is claimed once the store's clock moves the period on. It catches a claim that compares
// the duty's due time, which every server reads, with this server's clock.
#[test]
fn the_pass_is_claimed_for_a_period_of_the_store_clock() {
    let (rig, server) = two_clock_rig("clock_duty", LIMITS);
    rig.agents.set_duty_period(MINUTE);
    assert_eq!(ran(rig.agents.job_pass()), Some(true));
    advance_server(&server, HOUR);
    assert_eq!(
        ran(rig.agents.job_pass()),
        None,
        "claimed by the server's clock"
    );
    rig.advance(MINUTE);
    assert_eq!(ran(rig.agents.job_pass()), Some(true));
}

// A pass whose store cannot read its clock (a stopped database) decides nothing: with the
// job past its wait on both clocks, the pass answers the store's failure, the job stays queued
// and nothing is committed; with a duty period, the claim fails the same way and writes no
// duty row. Once the clock reads again the pass ends the job. It catches a pass that judges
// the deadlines by a reading it did not make (the epoch, which ends nothing, or the server's
// clock, which ends by another clock than the one that stamped them).
#[test]
fn a_pass_whose_store_cannot_read_its_clock_decides_nothing() {
    let (rig, server) = two_clock_rig("clock_stopped", LIMITS);
    let id = queue(&rig, "job-q", spec());
    rig.advance(HOUR);
    advance_server(&server, HOUR);
    rig.store.fail_with(stopped());
    rig.store.fail_clock(true);
    let commits = rig.commits();
    let failed = Some(Pass::Failed(FailureKind::Disconnected));
    assert_eq!(rig.agents.job_pass(), failed);
    rig.agents.set_duty_period(MINUTE);
    assert_eq!(rig.agents.job_pass(), failed);
    assert_eq!(rig.job(&id).state, JobState::Queued);
    assert_eq!(
        rig.commits(),
        commits,
        "a pass without the store's clock wrote"
    );

    rig.store.fail_clock(false);
    assert_eq!(ran(rig.agents.job_pass()), Some(true));
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
}

/// The job service over the rig's tables, on the server's clock.
fn api(rig: &Rig) -> JobApi {
    JobApi::new(Arc::clone(&rig.jobs), rig.shared.clone())
}

async fn get_result(api: &JobApi, id: &JobId) -> Result<JobResult, Status> {
    let asked = as_operator(Request::new(GetResultRequest {
        job_id: id.to_string(),
    }));
    let answer = api.get_result(asked).await?.into_inner();
    Ok(answer.result.unwrap())
}

// A result is read until its retention end by the store's clock, which stamped that end and
// by which every server's sweep deletes the job: with the server's clock past the end and the
// store's not, the result is read; with the store's past it, it is `expired`; and a store that
// cannot read its clock answers a failed read, neither the result nor `expired`. A job that
// has not ended is answered without the clock. It catches a read that answers `expired` by one
// server's clock for a job another server would still show, and one that answers from a
// reading it could not make.
#[tokio::test]
async fn a_result_is_read_until_its_retention_end_by_the_store_clock() {
    let (rig, server) = two_clock_rig("clock_retention", LIMITS);
    let api = api(&rig);
    let ended = rig.place("job-ended", "host-a");
    for (from, to) in [
        (JobState::Placed, JobState::Starting),
        (JobState::Starting, JobState::Running),
    ] {
        rig.jobs.transition(&ended, from, to).unwrap();
    }
    let exited = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    rig.jobs.end_job(&ended, exited).unwrap();
    let waiting = queue(&rig, "job-waiting", spec());
    let window = rig.jobs.retention().window();

    advance_server(&server, window + DAY);
    assert!(get_result(&api, &ended).await.is_ok());

    rig.store.fail_with(stopped());
    rig.store.fail_clock(true);
    let unread = get_result(&api, &ended).await.unwrap_err();
    assert_eq!(
        (unread.code(), unread.message()),
        (Code::Internal, "internal")
    );
    let unfinished = get_result(&api, &waiting).await.unwrap_err();
    assert_eq!(unfinished.message(), "not-finished");
    rig.store.fail_clock(false);

    rig.advance(window);
    let expired = get_result(&api, &ended).await.unwrap_err();
    assert_eq!(expired.message(), "expired");
}

// The whole-host start a heartbeat makes is timed by the store's clock: the first listing
// after a hello moves the placed job's prepare deadline to the store's clock plus the prepare
// timeout, whatever the server's clock reads (a day ahead here), so the pass of any server
// judges it by the clock that set it. A heartbeat whose store cannot read its clock is
// answered `500 state-write-failed` and moves nothing; the agent's next report restarts it.
// So is a refusal of the prepare, whose requeue stamps the job's bars: with the clock gone it
// is `500` and the job stays placed on its allocation; once the clock reads, the same report
// requeues it. It catches a deadline stamped with the server's clock, and a restart or a
// failed prepare stamped from a reading the store did not make.
#[tokio::test]
async fn a_hello_restarts_a_whole_host_prepare_on_the_store_clock() {
    let (mut rig, server) = two_clock_rig("clock_whole_start", LIMITS);
    rig.register("host-a", true);
    let mut agent = crate::agent_api::job_rig::FakeAgent::new("host-a");
    let id = JobId::parse("job-w").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: JobSpec {
            needs: js_jobs::Needs::WholeHost,
            ..spec()
        },
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    let placement = js_jobs::Placement {
        prepare_timeout: Some(HOUR),
        ..js_jobs::Placement::default()
    };
    let host = js_types::HostId::new("host-a").unwrap();
    let whole = js_jobs::Booked::WholeHost;
    rig.jobs.place_with(&id, host, whole, placement).unwrap();
    let placed = rig.allocation(&id).prepare_deadline;

    advance_server(&server, DAY);
    rig.advance(MINUTE);
    rig.register("host-a", true);
    rig.store.fail_with(stopped());
    rig.store.fail_clock(true);
    let refused = rig.beat(&agent).await.unwrap_err();
    let body = serde_json::to_value(&refused).unwrap();
    let status = axum::response::IntoResponse::into_response(refused).status();
    // The beat's own row write is stamped from the store's clock and is the first to need it:
    // 503, before the listing is read, and the deadline stays where it was.
    assert_eq!(
        (status.as_u16(), &body["error"]),
        (503, &serde_json::json!("state-write-failed"))
    );
    assert_eq!(rig.allocation(&id).prepare_deadline, placed);

    rig.store.fail_clock(false);
    // The deadline is read after the beat's own row write moved the store's clock on.
    let hello_at = js_state::Store::now(&*rig.store)
        .unwrap()
        .saturating_add(Duration::from_millis(2));
    rig.beat(&agent).await.unwrap();
    let timeout = Duration::from_millis(rig.agents.whole_rules().prepare_timeout_ms);
    assert_eq!(
        rig.allocation(&id).prepare_deadline,
        Some(hello_at.saturating_add(timeout))
    );

    let placed = rig.allocation(&id);
    agent.refused = Some(js_agent_wire::ExclusiveRefusal {
        lease_id: placed.allocation_id.to_string(),
        code: js_agent_wire::RefusalReason::NotQuiet,
        detail: String::new(),
    });
    rig.store.fail_clock(true);
    let commits = rig.commits();
    let refused = rig.beat(&agent).await.unwrap_err();
    let status = axum::response::IntoResponse::into_response(refused).status();
    // The beat's row write needs the clock first, so the refusal is its 503 and nothing at all
    // is written.
    assert_eq!(status.as_u16(), 503);
    assert_eq!(
        rig.commits(),
        commits,
        "a refused prepare wrote without the clock"
    );
    assert_eq!(rig.job(&id).state, JobState::Placed);
    assert_eq!(rig.allocation(&id), placed);

    rig.store.fail_clock(false);
    rig.beat(&agent).await.unwrap();
    let requeued = rig.job(&id);
    assert_eq!(
        (requeued.state, requeued.failed_prepares),
        (JobState::Queued, 1)
    );
}
