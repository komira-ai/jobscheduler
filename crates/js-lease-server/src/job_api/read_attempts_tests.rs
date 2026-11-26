//! What `GetJob` and `GetResult` show of a job's attempts (`read/view.rs`,
//! `docs/design/job-retry-and-completion-callback.md`, section 3.7): the current attempt, the
//! history of the ended ones field by field, when the next attempt may be placed (only while
//! the job is queued), the count of dropped attempts, the queue status of a job waiting for
//! its retry, the retry policy a spec echoes, and the attempt of a result. Every record is
//! produced through the job tables (a placement, `retry_txn`, a free, an end) and read over
//! both transports by the rig of `read_tests.rs`; nothing sleeps, the clock is moved by hand.

use std::time::Duration;

use js_jobs::{
    AttemptOutcome, AttemptRecord, EndReason, JobEnd, JobId, JobRecord, JobSpec, Output,
    ResultReason, StartFailure,
};
use js_proto::v1;
use js_state::Clock;
use js_types::{HostId, RetryPolicy};

use super::read::view;
use super::read_tests::{BOOKED, Rig, clock, json_of, over_fake, place, queue, run, spec, start};

/// The host called `name`.
fn host(name: &str) -> HostId {
    HostId::new(name).expect("a host id")
}

/// A spec that allows `max_attempts` attempts with waits of 5 s and 600 s.
fn retrying(max_attempts: u32) -> JobSpec {
    JobSpec {
        retry: Some(RetryPolicy {
            max_attempts,
            initial_backoff: Some(Duration::from_secs(5)),
            max_backoff: Some(Duration::from_secs(600)),
        }),
        ..spec(&[])
    }
}

/// The service over a fresh fake store.
async fn rig() -> Rig {
    let clock = clock();
    Rig::over(over_fake(&clock), clock).await
}

/// The store's time now, in milliseconds.
fn now_ms(rig: &Rig) -> u64 {
    view::millis(rig.clock.now())
}

/// The record of the job `id`.
fn record(rig: &Rig, id: &JobId) -> JobRecord {
    rig.jobs.get_job(id).expect("reads").expect("kept")
}

/// How an attempt of a job that exited with code 7 ended.
fn exited_with_seven() -> AttemptRecord {
    AttemptRecord {
        outcome: AttemptOutcome::Failed,
        exit_code: Some(7),
        reason: Some(ResultReason::Exit),
        start_failure: None,
        start_failure_detail: None,
    }
}

/// How an attempt that was placed and lost its host ended.
fn lost_host() -> AttemptRecord {
    AttemptRecord {
        outcome: AttemptOutcome::HostLost,
        exit_code: None,
        reason: None,
        start_failure: None,
        start_failure_detail: None,
    }
}

/// Places the queued job `id` on `at`, moves the clock on a second, queues its next attempt
/// after `how` the attempt ended, to be placed `wait` from now, and frees the allocation the
/// retry released. Every commit of the fake store moves the clock on, so the instants are
/// read back: it returns when the store placed the job and when the retry ended the attempt
/// (the clock's reading just before `retry_txn`, which commits nothing before it reads the
/// clock), in milliseconds. The wait ends `wait` after the second.
fn retried_on(rig: &Rig, id: &JobId, at: &str, how: AttemptRecord, wait: Duration) -> (u64, u64) {
    rig.jobs
        .place(id, host(at), BOOKED)
        .expect("the job is placed");
    let placed = record(rig, id).placed_at.map(view::millis);
    rig.advance(Duration::from_secs(1));
    let ended = now_ms(rig);
    let not_before = rig.clock.now().saturating_add(wait);
    rig.jobs
        .retry_txn(id, how, not_before)
        .expect("the job retries");
    rig.jobs
        .free(id, EndReason::HostAbsent)
        .expect("the allocation is freed");
    (placed.expect("a placement time"), ended)
}

/// The times of a history entry.
fn times(queued: u64, placed: u64, started: Option<u64>, ended: u64) -> Option<v1::JobTimes> {
    Some(v1::JobTimes {
        queued_at_ms: queued,
        placed_at_ms: Some(placed),
        started_at_ms: started,
        ended_at_ms: Some(ended),
    })
}

/// Asserts that a job never retried shows attempt 1, an empty history, no wait and nothing
/// dropped while queued, placed and running, and that its result, with a stored result and
/// without one (failed to start), shows attempt 1. Catches an attempt read as 0 for a row that
/// holds no attempt fields, a wait invented for a job never retried, and a result that
/// leaves its attempt unset.
#[tokio::test]
async fn a_job_never_retried_shows_attempt_one() {
    let rig = rig().await;
    let jobs = &rig.jobs;
    let plain = queue(jobs, "plain-1", spec(&[]));
    let shown = rig.get_job("plain-1").await.expect("read");
    let fields = |job: &v1::Job| {
        (
            job.attempt,
            job.attempt_history.clone(),
            job.retry_not_before_ms,
            job.dropped_attempts,
        )
    };
    assert_eq!(fields(&shown), (1, Vec::new(), None, 0));
    assert_eq!(shown.spec.expect("a spec").retry, None);
    place(jobs, &plain);
    assert_eq!(fields(&rig.get_job("plain-1").await.expect("read")).0, 1);
    start(jobs, &plain);
    run(jobs, &plain);
    let running = rig.get_job("plain-1").await.expect("read");
    assert_eq!(fields(&running), (1, Vec::new(), None, 0));

    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    let done = jobs.end_job(&plain, end).expect("the job ends");
    let result = rig.get_result("plain-1").await.expect("read");
    assert_eq!(result.attempt, 1);
    assert_eq!(result, view::result(&done));

    let never = queue(jobs, "never-1", spec(&[]));
    let end = JobEnd::FailedToStart(StartFailure::NoCapacity);
    let failed = jobs.end_job(&never, end).expect("the job ends");
    assert!(failed.result.is_none());
    let result = rig.get_result("never-1").await.expect("read");
    assert_eq!(result.attempt, 1);
    assert_eq!(result, view::result(&failed));
}

/// Asserts that a job whose first attempt ran on host-a and exited with code 7, and that was
/// queued for its retry 30 s on, shows state queued, attempt 1, one history entry with every
/// field (number, host, failed, the four times, exit code 7, reason exit, no start failure,
/// no detail, retried), the time its next attempt may be placed, the queue status
/// `WAITING_FOR_RETRY` with its detail and the `RETRY_SCHEDULED` event naming host-a, and
/// that the gateway names the fields in camel case. Catches a history field left unmapped, a
/// wait that is not shown while the job is queued, an event without its host, and a status
/// that stays at the wait before the retry.
#[tokio::test]
async fn a_job_waiting_for_its_retry_shows_the_attempt_that_ended_and_the_wait() {
    let rig = rig().await;
    let jobs = &rig.jobs;
    let job = queue(jobs, "wait-1", retrying(3));
    let queued = view::millis(record(&rig, &job).submitted_at);
    rig.advance(Duration::from_secs(1));
    place(jobs, &job);
    let placed = record(&rig, &job).placed_at.map(view::millis);
    assert!(placed.is_some());
    rig.advance(Duration::from_millis(500));
    start(jobs, &job);
    rig.advance(Duration::from_millis(500));
    run(jobs, &job);
    let started = record(&rig, &job).started_at.map(view::millis);
    assert!(started.is_some());
    rig.advance(Duration::from_secs(3));
    let ended = now_ms(&rig);
    let not_before = rig.clock.now().saturating_add(Duration::from_secs(30));
    jobs.retry_txn(&job, exited_with_seven(), not_before)
        .expect("the job retries");
    jobs.free(&job, EndReason::HostAbsent).expect("freed");

    let shown = rig.get_job("wait-1").await.expect("read");
    assert_eq!(shown.state, i32::from(v1::JobState::Queued));
    assert_eq!(shown.attempt, 1);
    assert_eq!(
        shown.attempt_history,
        [v1::Attempt {
            number: 1,
            host_id: "host-a".to_owned(),
            outcome: v1::AttemptOutcome::Failed.into(),
            times: times(queued, placed.expect("placed"), started, ended),
            exit_code: Some(7),
            reason: v1::ResultReason::Exit.into(),
            start_failure: v1::StartFailureCode::Unspecified.into(),
            start_failure_detail: String::new(),
            retried: true,
        }]
    );
    assert_eq!(shown.retry_not_before_ms, Some(ended + 30_000));
    assert_eq!(shown.dropped_attempts, 0);
    let detail = "attempt 1 on host host-a ended: it ended; attempt 2 may be placed in 30 s";
    assert_eq!(
        shown.queue_status,
        Some(v1::QueueStatus {
            code: v1::QueueStatusCode::WaitingForRetry.into(),
            detail: detail.to_owned(),
            since_ms: ended,
        })
    );
    assert_eq!(
        shown.events.last(),
        Some(&v1::JobEvent {
            at_ms: ended,
            kind: v1::JobEventKind::RetryScheduled.into(),
            detail: detail.to_owned(),
            host_id: Some("host-a".to_owned()),
        })
    );

    let (_, body) = rig.get("/v1/jobs/wait-1").await;
    let json = json_of(&body);
    let job = &json["job"];
    assert_eq!(job["attempt"], 1);
    assert_eq!(job["retryNotBeforeMs"], (ended + 30_000).to_string());
    assert_eq!(
        job["attemptHistory"][0]["outcome"],
        "ATTEMPT_OUTCOME_FAILED"
    );
    assert_eq!(job["attemptHistory"][0]["exitCode"], 7);
    assert_eq!(job["attemptHistory"][0]["retried"], true);
    assert_eq!(
        job["queueStatus"]["code"],
        "QUEUE_STATUS_CODE_WAITING_FOR_RETRY"
    );
}

/// Asserts that the next placement of a retried job counts attempt 2 and takes the wait off
/// the job (the record keeps it), that the history still holds the first attempt, that a
/// second retry after an attempt that never started appends an entry with no start time and
/// its start failure and agent code, whose queued time is the end of the wait before it, and
/// that the third attempt, placed on a third host, shows attempt 3 once running. Catches an
/// attempt number that stays behind the placement, a wait shown for a job past the queue, and
/// an attempt that never ran given a start time.
#[tokio::test]
async fn the_next_placement_counts_the_next_attempt_and_hides_the_wait() {
    let rig = rig().await;
    let jobs = &rig.jobs;
    let job = queue(jobs, "next-1", retrying(3));
    rig.advance(Duration::from_secs(1));
    let (_, first_ended) = retried_on(&rig, &job, "host-a", lost_host(), Duration::from_secs(10));
    let wait_ends = first_ended + 10_000;
    let waiting = rig.get_job("next-1").await.expect("read");
    assert_eq!(
        (waiting.attempt, waiting.retry_not_before_ms),
        (1, Some(wait_ends))
    );

    rig.advance(Duration::from_secs(20));
    rig.jobs
        .place(&job, host("host-b"), BOOKED)
        .expect("placed");
    let placed_at = view::millis(record(&rig, &job).placed_at.expect("a placement time"));
    let placed = rig.get_job("next-1").await.expect("read");
    assert_eq!(placed.state, i32::from(v1::JobState::Placed));
    assert_eq!(placed.attempt, 2);
    assert_eq!(placed.retry_not_before_ms, None);
    assert_eq!(placed.attempt_history.len(), 1);
    assert!(record(&rig, &job).attempt.retry_not_before().is_some());

    rig.advance(Duration::from_secs(2));
    let ended_at = now_ms(&rig);
    let refused = AttemptRecord {
        outcome: AttemptOutcome::StartFailed,
        exit_code: None,
        reason: None,
        start_failure: Some(StartFailure::AgentRefused),
        start_failure_detail: Some("disk-full".to_owned()),
    };
    let not_before = rig.clock.now().saturating_add(Duration::from_secs(5));
    jobs.retry_txn(&job, refused, not_before).expect("retries");
    jobs.free(&job, EndReason::HostAbsent).expect("freed");
    let again = rig.get_job("next-1").await.expect("read");
    assert_eq!(again.attempt, 2);
    assert_eq!(again.retry_not_before_ms, Some(ended_at + 5000));
    assert_eq!(
        again.attempt_history[1],
        v1::Attempt {
            number: 2,
            host_id: "host-b".to_owned(),
            outcome: v1::AttemptOutcome::StartFailed.into(),
            times: times(wait_ends, placed_at, None, ended_at),
            exit_code: None,
            reason: v1::ResultReason::Unspecified.into(),
            start_failure: v1::StartFailureCode::AgentRefused.into(),
            start_failure_detail: "disk-full".to_owned(),
            retried: true,
        }
    );
    assert_eq!(again.attempt_history[0].number, 1);
    assert_eq!(again.attempt_history[0].host_id, "host-a");

    rig.advance(Duration::from_secs(6));
    rig.jobs
        .place(&job, host("host-c"), BOOKED)
        .expect("placed");
    start(jobs, &job);
    run(jobs, &job);
    let running = rig.get_job("next-1").await.expect("read");
    assert_eq!(running.state, i32::from(v1::JobState::Running));
    assert_eq!(running.attempt, 3);
    assert_eq!(running.retry_not_before_ms, None);
    assert_eq!(running.attempt_history.len(), 2);
}

/// One ended attempt a case of [`every_outcome_and_start_failure_is_shown`] gives to a job
/// and what it must read as.
type Mapped = (AttemptRecord, v1::Attempt);

/// The cases: each outcome, each start failure and none, an exit code with and without a
/// reason of each kind, and a start failure detail with and without text.
fn mapped() -> Vec<Mapped> {
    use v1::{AttemptOutcome as O, ResultReason as R, StartFailureCode as F};
    let case = |outcome,
                exit_code,
                reason,
                start_failure,
                detail: Option<&str>,
                (shown, code, why, failure): (O, Option<i32>, R, F)| {
        let record = AttemptRecord {
            outcome,
            exit_code,
            reason,
            start_failure,
            start_failure_detail: detail.map(str::to_owned),
        };
        let attempt = v1::Attempt {
            outcome: shown.into(),
            exit_code: code,
            reason: why.into(),
            start_failure: failure.into(),
            start_failure_detail: detail.unwrap_or_default().to_owned(),
            ..v1::Attempt::default()
        };
        (record, attempt)
    };
    use AttemptOutcome as A;
    use ResultReason as P;
    use StartFailure as S;
    vec![
        case(
            A::Succeeded,
            Some(0),
            Some(P::Exit),
            None,
            None,
            (O::Succeeded, Some(0), R::Exit, F::Unspecified),
        ),
        case(
            A::Failed,
            Some(2),
            Some(P::Exit),
            None,
            None,
            (O::Failed, Some(2), R::Exit, F::Unspecified),
        ),
        case(
            A::Failed,
            None,
            Some(P::WallLimit),
            None,
            None,
            (O::Failed, None, R::WallLimit, F::Unspecified),
        ),
        case(
            A::Failed,
            None,
            Some(P::Memory),
            None,
            None,
            (O::Failed, None, R::Memory, F::Unspecified),
        ),
        case(
            A::Failed,
            None,
            Some(P::InputFailed),
            None,
            None,
            (O::Failed, None, R::InputFailed, F::Unspecified),
        ),
        case(
            A::Cancelled,
            Some(137),
            Some(P::Cancelled),
            None,
            None,
            (O::Cancelled, Some(137), R::Cancelled, F::Unspecified),
        ),
        case(
            A::HostLost,
            None,
            None,
            None,
            None,
            (O::HostLost, None, R::Unspecified, F::Unspecified),
        ),
        case(
            A::StartFailed,
            None,
            None,
            Some(S::NoCapacity),
            None,
            (O::StartFailed, None, R::Unspecified, F::NoCapacity),
        ),
        case(
            A::StartFailed,
            None,
            None,
            Some(S::StartTimeout),
            None,
            (O::StartFailed, None, R::Unspecified, F::StartTimeout),
        ),
        case(
            A::StartFailed,
            None,
            None,
            Some(S::ExclusiveNotReady),
            None,
            (O::StartFailed, None, R::Unspecified, F::ExclusiveNotReady),
        ),
        case(
            A::StartFailed,
            None,
            None,
            Some(S::AgentRefused),
            Some("bad-mount"),
            (O::StartFailed, None, R::Unspecified, F::AgentRefused),
        ),
        case(
            A::StartFailed,
            None,
            None,
            Some(S::InputUnavailable),
            Some("input-unavailable"),
            (O::StartFailed, None, R::Unspecified, F::InputUnavailable),
        ),
    ]
}

/// Asserts that an ended attempt is shown with its outcome (each of the five), its exit code
/// (a code, none), its reason (each of the five, none shown as unspecified), its start
/// failure (each of the five, none shown as unspecified) and its agent code (given, absent
/// shown as empty), every other field of the entry unchanged by the case. Catches an outcome,
/// a start failure or a reason mapped to a neighbour, and an absent field shown as a value.
#[tokio::test]
async fn every_outcome_and_start_failure_is_shown() {
    let rig = rig().await;
    for (n, (how, expected)) in mapped().into_iter().enumerate() {
        let text = format!("map-{n}");
        let job = queue(&rig.jobs, &text, retrying(2));
        let queued_at = view::millis(record(&rig, &job).submitted_at);
        rig.advance(Duration::from_secs(1));
        let (placed_at, ended_at) = retried_on(&rig, &job, "host-a", how, Duration::from_secs(5));
        let shown = rig.get_job(&text).await.expect("read");
        let expected = v1::Attempt {
            number: 1,
            host_id: "host-a".to_owned(),
            times: times(queued_at, placed_at, None, ended_at),
            retried: true,
            ..expected
        };
        assert_eq!(shown.attempt_history, [expected], "{text}");
    }
}

/// Asserts that a job that ended while it waited for its retry (its queue wait ran out) shows
/// its end state and no wait, though its record keeps the time, with its first attempt in the
/// history still marked retried, attempt 1, and a result without a stored result showing
/// attempt 1. Catches a wait shown on a job past the queue, and a history cleared at the end.
#[tokio::test]
async fn a_job_that_ended_while_it_waited_shows_no_wait_and_keeps_its_history() {
    let rig = rig().await;
    let jobs = &rig.jobs;
    let job = queue(jobs, "gone-1", retrying(2));
    retried_on(&rig, &job, "host-a", lost_host(), Duration::from_secs(60));
    let waiting = rig.get_job("gone-1").await.expect("read");
    assert!(waiting.retry_not_before_ms.is_some());
    let end = JobEnd::FailedToStart(StartFailure::NoCapacity);
    let ended = jobs.end_job(&job, end).expect("the job ends");
    assert!(ended.attempt.retry_not_before().is_some());
    let shown = rig.get_job("gone-1").await.expect("read");
    assert_eq!(shown.state, i32::from(v1::JobState::FailedToStart));
    assert_eq!(shown.retry_not_before_ms, None);
    assert_eq!(shown.attempt, 1);
    assert_eq!(shown.attempt_history.len(), 1);
    assert!(shown.attempt_history[0].retried);
    assert_eq!(
        shown.attempt_history[0].outcome,
        i32::from(v1::AttemptOutcome::HostLost)
    );
    let result = rig.get_result("gone-1").await.expect("read");
    assert_eq!(result.attempt, 1);
    assert_eq!(result, view::result(&ended));
}

/// Asserts that the result of a job that ran a second attempt shows attempt 2 with a stored
/// result (it exited) and without one (it failed to start on its second placement), and that
/// the job shows the same attempt. Catches a result that shows the first attempt, and an
/// attempt read from the history's length instead of the record's current attempt.
#[tokio::test]
async fn the_result_of_a_retried_job_shows_its_attempt() {
    let rig = rig().await;
    let jobs = &rig.jobs;
    let exited = queue(jobs, "res-1", retrying(2));
    retried_on(&rig, &exited, "host-a", lost_host(), Duration::from_secs(5));
    rig.advance(Duration::from_secs(5));
    place(jobs, &exited);
    start(jobs, &exited);
    run(jobs, &exited);
    let end = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    let done = jobs.end_job(&exited, end).expect("ends");
    let result = rig.get_result("res-1").await.expect("read");
    assert_eq!(result.attempt, 2);
    assert_eq!(result, view::result(&done));
    assert_eq!(rig.get_job("res-1").await.expect("read").attempt, 2);

    let refused = queue(jobs, "res-2", retrying(2));
    retried_on(
        &rig,
        &refused,
        "host-a",
        lost_host(),
        Duration::from_secs(5),
    );
    rig.advance(Duration::from_secs(5));
    place(jobs, &refused);
    let end = JobEnd::FailedToStart(StartFailure::StartTimeout);
    let failed = jobs.end_job(&refused, end).expect("ends");
    assert!(failed.result.is_none());
    let result = rig.get_result("res-2").await.expect("read");
    assert_eq!(result.attempt, 2);
    assert_eq!(result, view::result(&failed));
    assert_eq!(rig.get_job("res-2").await.expect("read").attempt, 2);
}

/// Asserts that a job retried 260 times shows its newest 256 attempts, numbered 5 to 260 in
/// order, the 4 older ones counted in `dropped_attempts`, and attempt 260. Catches a history
/// shown whole past its bound, a dropped count left at 0 and a history shown oldest dropped
/// from the wrong end.
#[tokio::test]
async fn the_history_shows_the_newest_attempts_and_counts_the_dropped() {
    let rig = rig().await;
    let job = queue(&rig.jobs, "many-1", retrying(u32::MAX));
    let hosts = ["host-a", "host-b", "host-c"];
    for n in 0..260 {
        retried_on(
            &rig,
            &job,
            hosts[n % 3],
            lost_host(),
            Duration::from_secs(1),
        );
    }
    let shown = rig.get_job("many-1").await.expect("read");
    let numbers: Vec<u32> = shown.attempt_history.iter().map(|a| a.number).collect();
    assert_eq!(numbers, (5..=260).collect::<Vec<u32>>());
    assert_eq!(shown.dropped_attempts, 4);
    assert_eq!(shown.attempt, 260);
}

/// Asserts that a spec echoes its retry policy as the record keeps it: the attempts, and the
/// waits in whole seconds with an absent wait shown absent (not 0), a policy of 0 attempts
/// with no waits shown as it is, a job without a policy shown with none, and a wait that does
/// not fit 32 bits shown as the largest value. Catches a wait shown as 0 for absent, a policy
/// dropped from the view, and a wide wait that wraps.
#[tokio::test]
async fn the_spec_echoes_the_retry_policy_in_seconds() {
    let rig = rig().await;
    let policy = |max_attempts: u32, initial: Option<u64>, max: Option<u64>| JobSpec {
        retry: Some(RetryPolicy {
            max_attempts,
            initial_backoff: initial.map(Duration::from_secs),
            max_backoff: max.map(Duration::from_secs),
        }),
        ..spec(&[])
    };
    let cases = [
        (
            "echo-1",
            policy(4, Some(5), Some(600)),
            Some((4, Some(5), Some(600))),
        ),
        (
            "echo-2",
            policy(2, Some(3600), None),
            Some((2, Some(3600), None)),
        ),
        (
            "echo-3",
            policy(3, None, Some(90)),
            Some((3, None, Some(90))),
        ),
        ("echo-4", policy(0, None, None), Some((0, None, None))),
        ("echo-5", spec(&[]), None),
    ];
    for (name, spec, expected) in cases {
        queue(&rig.jobs, name, spec);
        let shown = rig.get_job(name).await.expect("read").spec.expect("a spec");
        let expected = expected.map(|(max_attempts, initial, max)| v1::RetryPolicy {
            max_attempts,
            initial_backoff_seconds: initial,
            max_backoff_seconds: max,
        });
        assert_eq!(shown.retry, expected, "{name}");
    }
    let (_, body) = rig.get("/v1/jobs/echo-1").await;
    let json = json_of(&body);
    let retry = &json["job"]["spec"]["retry"];
    assert_eq!(retry["maxAttempts"], 4);
    assert_eq!(retry["initialBackoffSeconds"], 5);
    assert_eq!(retry["maxBackoffSeconds"], 600);

    let wide = queue(&rig.jobs, "wide-1", policy(2, Some(5), Some(5)));
    let mut kept = record(&rig, &wide);
    kept.spec.retry = Some(RetryPolicy {
        max_attempts: 2,
        initial_backoff: Some(Duration::from_secs(u64::MAX)),
        max_backoff: Some(Duration::from_secs(u64::from(u32::MAX) + 1)),
    });
    let shown = view::job(&kept).spec.expect("a spec").retry;
    assert_eq!(
        shown,
        Some(v1::RetryPolicy {
            max_attempts: 2,
            initial_backoff_seconds: Some(u32::MAX),
            max_backoff_seconds: Some(u32::MAX),
        })
    );
}
