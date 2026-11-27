//! What the reference job service shows of a job's attempts, beside what the suite's
//! scenarios hold every service to: the moves that end an attempt refuse a job they do not
//! start from, a service that breaks a rule still refuses and ends as the rest of it does,
//! and the times and the status of a wait are the real service's.

use std::time::Duration;

use js_conformance::jobs::{
    self, Defect, JobHarness, ReferenceHarness, ReferenceJobs, Refused, Step,
};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::{self, JobState};
use tonic::transport::Channel;

/// A harness over `service` and a client of it, the job `spec` submitted.
async fn with_job(
    service: ReferenceJobs,
    spec: v1::JobSpec,
) -> (ReferenceHarness, JobServiceClient<Channel>) {
    let mut harness = ReferenceHarness::of(service);
    let mut client = harness.client();
    client
        .submit_job(jobs::submit(spec))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    (harness, client)
}

async fn job_of(client: &mut JobServiceClient<Channel>, id: &str) -> v1::Job {
    let request = v1::GetJobRequest {
        job_id: id.to_string(),
    };
    let answer = client.get_job(request).await;
    let answer = answer.unwrap_or_else(|e| panic!("{e}"));
    answer.into_inner().job.unwrap_or_default()
}

/// A job with a retry policy of `max_attempts` attempts and the default waits.
fn retrying(id: &str, max_attempts: u32) -> v1::JobSpec {
    v1::JobSpec {
        retry: Some(v1::RetryPolicy {
            max_attempts,
            initial_backoff_seconds: None,
            max_backoff_seconds: None,
        }),
        ..jobs::spec(id)
    }
}

/// Asserts that the moves that end an attempt or deliver a callback refuse a job no service
/// has, and refuse a job in no state they start from, leaving it as it was: a queued job is
/// not lost, fails no start and has no callback to deliver, and a running job has no start
/// to fail. Catches a hook that answers for a job that is not there or not live.
#[tokio::test]
async fn the_attempt_moves_refuse_an_unknown_job_and_a_job_in_no_state_they_start_from() {
    let (mut harness, mut client) = with_job(ReferenceJobs::new("c"), retrying("queued", 3)).await;
    for step in [Step::HostLost, Step::StartFailed, Step::DeliverCallback] {
        assert_eq!(harness.step("nobody", step), Err(Refused), "{step:?}");
        assert_eq!(harness.step("queued", step), Err(Refused), "{step:?}");
    }
    for step in [Step::Place, Step::Start, Step::Run] {
        assert_eq!(harness.step("queued", step), Ok(()), "{step:?}");
    }
    assert_eq!(harness.step("queued", Step::StartFailed), Err(Refused));
    let job = job_of(&mut client, "queued").await;
    assert_eq!(job.state(), JobState::Running);
}

/// Asserts that a service that retries a nonzero exit by defect still refuses the exit of a
/// job that is not running, and ends a job that has no policy failed with the exit code
/// recorded. Catches an exit that is retried without a policy or a state to retry from.
#[tokio::test]
async fn a_service_that_retries_an_exit_still_ends_a_job_with_no_attempt_left() {
    let service = ReferenceJobs::broken(Defect::ExitRetries);
    let (mut harness, mut client) = with_job(service, jobs::spec("bare")).await;
    assert_eq!(harness.step("bare", Step::Exit(2)), Err(Refused));
    for step in [Step::Place, Step::Start, Step::Run, Step::Exit(2)] {
        assert_eq!(harness.step("bare", step), Ok(()), "{step:?}");
    }
    let job = job_of(&mut client, "bare").await;
    assert_eq!((job.state(), job.attempt), (JobState::Failed, 1));
    assert_eq!(job.attempt_history.len(), 1);
    assert_eq!(job.attempt_history[0].exit_code, Some(2));
}

/// Asserts that a retried job shows the times of the attempt that ended (queued at the
/// submit, placed, started and ended), the queue status that says why it waits and for how
/// long, a retry time that is gone once the next attempt is placed, and, for the attempt
/// after, a queued time that is the end of the wait. Catches a history entry with the wrong
/// times and a wait that is not shown or not cleared.
#[tokio::test]
async fn a_retried_job_shows_the_times_and_the_status_of_its_wait() {
    let (mut harness, mut client) = with_job(ReferenceJobs::new("c"), retrying("timed", 3)).await;
    harness.advance(Duration::from_secs(5));
    assert_eq!(harness.step("timed", Step::Place), Ok(()));
    harness.advance(Duration::from_secs(5));
    assert_eq!(harness.step("timed", Step::Start), Ok(()));
    assert_eq!(harness.step("timed", Step::Run), Ok(()));
    harness.advance(Duration::from_secs(5));
    assert_eq!(harness.step("timed", Step::HostLost), Ok(()));
    let job = job_of(&mut client, "timed").await;
    let first = &job.attempt_history[0];
    assert_eq!(first.host_id, "conformance-host");
    let times = first.times.unwrap_or_default();
    let shown = (
        times.queued_at_ms,
        times.placed_at_ms,
        times.started_at_ms,
        times.ended_at_ms,
    );
    assert_eq!(shown, (0, Some(5_000), Some(10_000), Some(15_000)));
    let until = job.retry_not_before_ms.unwrap_or_default();
    assert!((25_000..=26_000).contains(&until), "{until}");
    let status = job.queue_status.clone().unwrap_or_default();
    assert_eq!(status.since_ms, 15_000);
    let detail =
        "attempt 1 on host conformance-host ended: its host was lost; attempt 2 may be placed in 1";
    assert!(status.detail.starts_with(detail), "{}", status.detail);
    assert_eq!(job.times.unwrap_or_default().placed_at_ms, None);

    harness.advance(Duration::from_secs(60));
    assert_eq!(harness.step("timed", Step::Place), Ok(()));
    let job = job_of(&mut client, "timed").await;
    assert_eq!(job.state(), JobState::Placed);
    assert_eq!(
        (job.attempt, job.retry_not_before_ms, job.queue_status),
        (2, None, None)
    );
    assert_eq!(harness.step("timed", Step::Start), Ok(()));
    assert_eq!(harness.step("timed", Step::HostLost), Ok(()));
    let job = job_of(&mut client, "timed").await;
    let second = job.attempt_history[1].times.unwrap_or_default();
    assert_eq!(second.queued_at_ms, until);
}
