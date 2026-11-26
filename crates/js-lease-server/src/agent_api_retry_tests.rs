//! Retry of a job's attempt, end to end through the route and the job pass (design section 9,
//! `docs/design/job-retry-and-completion-callback.md`): a server that places with `js-sched`,
//! hosts that register and beat, and fake agents, all on one manual clock that only a test
//! moves ([`Rig::advance`]). They cover the wait before the next attempt, how the queue wait
//! counts for it, the bar on the host of the attempt that failed and the `start-fence`
//! capability that lifts it, the previous allocation that the next attempt waits for, and a
//! silent host that holds it.

use std::time::Duration;

use js_agent_wire::Outcome;
use js_jobs::{AllocationStep, EndReason, JobId, JobSpec, JobState, NewJob, StartFailure};
use js_proto::v1;
use js_types::RetryPolicy;
use serde_json::json;

use super::job_rig::{FakeAgent, Rig, clocked_rig, spec};
use crate::job_api::read::view;
use crate::job_expiry::JobLimits;

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// The wait before a retried job's next attempt: the policy's initial and longest wait alike,
/// so that no jitter shows.
const BACKOFF: Duration = Duration::from_secs(30);

/// How long a beat-by-beat wait steps between two rounds of beats: below the registry's grace
/// (15 s), so a host that beats each round is never silent.
const STEP: Duration = Duration::from_secs(10);

/// The agent's code of a start it refused for a cause of the host (not of the job).
const HOST_REFUSAL: &str = "vm-unavailable";

/// Limits under which only the queue wait of `queue_wait` can end a job: the start timeout is
/// a day.
fn limits(queue_wait: Duration) -> JobLimits {
    JobLimits {
        max_queue_wait: queue_wait,
        start_timeout: DAY,
        ..JobLimits::DEFAULT
    }
}

/// A job that retries a start that failed for its host: three attempts, [`BACKOFF`] between.
fn retrying() -> JobSpec {
    JobSpec {
        retry: Some(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Some(BACKOFF),
            max_backoff: Some(BACKOFF),
        }),
        ..spec()
    }
}

/// Records the queued job `id` with [`retrying`].
fn queue(rig: &Rig, id: &str) -> JobId {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: retrying(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    id
}

/// The capabilities of a host whose agent reports `start-fence`.
fn fenced() -> serde_json::Value {
    json!({ "per-job": "true", "arch": "x86_64", "start-fence": "true" })
}

/// The milliseconds of the rig's clock now.
fn now_ms(rig: &Rig) -> u64 {
    js_types::Clock::now(&rig.shared).millis()
}

/// When the next attempt of the retried job `id` may be placed, in the milliseconds of the
/// rig's clock: what its retry stored. The fake store moves the clock on with each commit it
/// makes durable, so a test reads the stored instant rather than add the backoff to a reading.
fn not_before_ms(rig: &Rig, id: &JobId) -> u64 {
    view::millis(rig.job(id).attempt.retry_not_before().unwrap())
}

/// Moves the clock on to `at`, in the milliseconds of the rig's clock; nothing when it is
/// there or past it.
fn advance_to(rig: &Rig, at: u64) {
    rig.advance(Duration::from_millis(at.saturating_sub(now_ms(rig))));
}

/// One heartbeat of `agent`, and the reply taken.
async fn beat(rig: &Rig, agent: &mut FakeAgent) {
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
}

/// Moves the clock on by `total`, in rounds of at most [`STEP`]: each round advances the clock,
/// runs a tick, has each of `deaf` beat (their replies ignored: a host that holds on to what
/// it has) and each of `listening` beat and take its reply, so no host that beats is silent.
async fn pass_time(
    rig: &Rig,
    total: Duration,
    listening: &mut [&mut FakeAgent],
    deaf: &[&FakeAgent],
) {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(STEP);
        left -= step;
        rig.advance(step);
        rig.tick();
        for agent in deaf {
            rig.beat(agent).await.unwrap();
        }
        for agent in listening.iter_mut() {
            let reply = rig.beat(agent).await.unwrap();
            agent.take(&reply);
        }
    }
}

/// Records the retrying job `id`, has the engine place it on `agent`'s host (the only one
/// registered) and walks it to starting there.
async fn start(rig: &Rig, agent: &mut FakeAgent, id: &str) -> JobId {
    let job = queue(rig, id);
    rig.tick();
    rig.tick();
    beat(rig, agent).await;
    beat(rig, agent).await;
    assert_eq!(rig.job(&job).state, JobState::Starting);
    job
}

/// The agent refuses the start of `id` for a cause of its host and the beat that lists the
/// refusal commits it: the job is queued for its next attempt and its allocation releases.
async fn refuse(rig: &Rig, agent: &mut FakeAgent, id: &JobId) {
    let ended = (Outcome::FailedToStart, None);
    agent.end_naming(id.as_str(), ended, (Some(HOST_REFUSAL), None));
    beat(rig, agent).await;
    assert_eq!(rig.job(id).state, JobState::Queued);
    assert_eq!(rig.allocation(id).step, AllocationStep::Releasing);
}

/// The agent's next beat no longer lists the refused job: its allocation is freed.
async fn free(rig: &Rig, agent: &mut FakeAgent, id: &JobId) {
    beat(rig, agent).await;
    assert_eq!(rig.allocation(id).step, AllocationStep::Freed);
}

/// Asserts that a job whose attempt failed in a way that retries is not lost to the queue's
/// order: it shows `WAITING_FOR_RETRY` with the attempt that ended and its not-before, is not
/// placed (although another host is idle and its previous allocation is freed) until its
/// backoff has passed, and is then placed there, as attempt 2. Catches a retry that is placed
/// at once, one that is placed late, an attempt number that does not count the placement, and
/// a retry shown with no status, no history or no not-before.
#[tokio::test]
async fn a_retried_job_waits_out_its_backoff() {
    let mut rig = clocked_rig("retry_backoff", true, limits(DAY));
    rig.register("host-a", true);
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-r").await;
    assert_eq!(rig.job(&id).attempt.number(), 1);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-a");

    refuse(&rig, &mut a, &id).await;
    free(&rig, &mut a, &id).await;
    let not_before = not_before_ms(&rig, &id);
    let shown = view::job(&rig.job(&id));
    assert_eq!(shown.attempt, 1);
    assert_eq!(shown.retry_not_before_ms, Some(not_before));
    let status = shown.queue_status.unwrap();
    assert_eq!(status.code, i32::from(v1::QueueStatusCode::WaitingForRetry));
    assert_eq!(
        status.detail,
        "attempt 1 on host host-a ended: it failed to start; attempt 2 may be placed in 30 s"
    );
    assert_eq!(shown.attempt_history.len(), 1);
    let ended = &shown.attempt_history[0];
    assert_eq!(ended.number, 1);
    assert_eq!(ended.host_id, "host-a");
    assert_eq!(ended.outcome, i32::from(v1::AttemptOutcome::StartFailed));
    assert_eq!(ended.start_failure_detail, HOST_REFUSAL);
    assert!(ended.retried);
    let ended_at = ended.times.and_then(|times| times.ended_at_ms);
    assert_eq!(ended_at.map(|ended| ended + 30_000), Some(not_before));

    // Another host is idle from now on; the job is held back by its backoff alone.
    rig.register("host-b", true);
    let mut b = FakeAgent::new("host-b");
    pass_time(&rig, 20 * SECOND, &mut [&mut a, &mut b], &[]).await;
    advance_to(&rig, not_before - 1);
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Queued);
    assert_eq!(rig.allocation(&id).step, AllocationStep::Freed);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-a");

    advance_to(&rig, not_before);
    rig.tick();
    rig.tick();
    let placed = rig.job(&id);
    assert_eq!(placed.state, JobState::Placed);
    assert_eq!(placed.attempt.number(), 2);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
    let shown = view::job(&placed);
    assert_eq!(shown.attempt, 2);
    assert_eq!(
        shown.retry_not_before_ms, None,
        "a placed job waits for nothing"
    );
    assert_eq!(shown.attempt_history.len(), 1);
}

/// Asserts that the queue wait of a retried job counts from the end of its backoff: with a
/// queue wait of five minutes and a backoff of 30 seconds, the job is still queued five
/// minutes after its submit and 1 ms before the backoff's end plus the wait, and ends
/// `NO_CAPACITY` at that instant. Catches a queue wait counted from the submit (the job would
/// end at five minutes, though it waited for its retry for 30 s of them), and one that ends a
/// millisecond late or early.
#[tokio::test]
async fn queue_wait_of_an_attempt_counts_from_its_backoff_end() {
    let queue_wait = 5 * MINUTE;
    let mut rig = clocked_rig("retry_queue_wait", true, limits(queue_wait));
    rig.register("host-a", true);
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-w").await;
    refuse(&rig, &mut a, &id).await;
    let submitted = view::millis(rig.job(&id).submitted_at);
    let wait_ends = not_before_ms(&rig, &id) + 300_000;

    // No host beats from here on: nothing can place the job, only the queue wait can end it.
    advance_to(&rig, submitted + 300_000);
    rig.tick();
    assert_eq!(
        rig.job(&id).state,
        JobState::Queued,
        "the wait of its first attempt is not the wait of this one"
    );
    advance_to(&rig, wait_ends - 1);
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Queued);

    advance_to(&rig, wait_ends);
    rig.tick();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
    let shown = view::job(&rig.job(&id));
    assert_eq!(
        shown.start_failure,
        i32::from(v1::StartFailureCode::NoCapacity)
    );
    assert_eq!(shown.attempt, 1, "no second attempt was placed");
}

/// Asserts that a retry is kept off the host of the attempt that failed for 15 minutes, and
/// that the host takes a later attempt of the job only when its hello reported `start-fence`.
/// (1) A host that reports it takes the retry back after the 15 minutes and not before it.
/// (2) A host that does not never takes it: with its bar over and the host idle, the job waits
/// until its queue wait ends `NO_CAPACITY`. (3) With a second host, the retry goes there as
/// soon as its backoff ends, though the first host reports `start-fence`. Catches a bar that
/// is absent or longer than 15 minutes, a host without the capability that gets a job whose
/// earlier attempt ran there, and a retry that waits for the first host's bar while another
/// host is free.
#[tokio::test]
async fn a_retry_bars_its_failed_host_then_needs_start_fence_to_return() {
    // (1) The only host reports start-fence.
    let mut rig = clocked_rig("retry_fence_on", true, limits(DAY));
    rig.register_with("host-a", fenced());
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-f").await;
    refuse(&rig, &mut a, &id).await;
    free(&rig, &mut a, &id).await;
    pass_time(&rig, 15 * MINUTE - SECOND, &mut [&mut a], &[]).await;
    assert_eq!(
        rig.job(&id).state,
        JobState::Queued,
        "barred for 15 minutes"
    );
    pass_time(&rig, 2 * SECOND, &mut [&mut a], &[]).await;
    rig.tick();
    let placed = rig.job(&id);
    assert_eq!(placed.state, JobState::Placed);
    assert_eq!(placed.attempt.number(), 2);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-a");

    // (2) The only host does not report it.
    let queue_wait = 20 * MINUTE;
    let mut rig = clocked_rig("retry_fence_off", true, limits(queue_wait));
    rig.register("host-a", true);
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-f").await;
    refuse(&rig, &mut a, &id).await;
    free(&rig, &mut a, &id).await;
    pass_time(&rig, 16 * MINUTE, &mut [&mut a], &[]).await;
    assert_eq!(
        rig.job(&id).state,
        JobState::Queued,
        "the bar is over, the host idle and alive, and the job still not placed there"
    );
    let wait_ends = not_before_ms(&rig, &id) + 20 * 60_000;
    pass_time(&rig, 3 * MINUTE, &mut [&mut a], &[]).await;
    advance_to(&rig, wait_ends - 1);
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Queued);
    advance_to(&rig, wait_ends);
    rig.tick();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
    assert_eq!(rig.job(&id).attempt.number(), 1, "never placed again");

    // (3) Another host takes the retry as soon as its backoff ends.
    let mut rig = clocked_rig("retry_fence_other", true, limits(DAY));
    rig.register_with("host-a", fenced());
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-f").await;
    refuse(&rig, &mut a, &id).await;
    free(&rig, &mut a, &id).await;
    rig.register("host-b", true);
    let mut b = FakeAgent::new("host-b");
    let not_before = not_before_ms(&rig, &id);
    pass_time(&rig, 20 * SECOND, &mut [&mut a, &mut b], &[]).await;
    advance_to(&rig, not_before - 1);
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Queued);
    advance_to(&rig, not_before);
    rig.tick();
    rig.tick();
    assert_eq!(rig.job(&id).state, JobState::Placed);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
}

/// Asserts that the next attempt of a job whose start timed out waits for the previous
/// attempt's allocation: the host still holds the container, so the allocation stays
/// releasing, and the job is not placed on an idle host after its backoff has passed; once the
/// host's listing drops the job the allocation is freed with the end reason `StartTimeout` and
/// the job is placed, as attempt 2. Catches a placement that books a second allocation for the
/// job beside the one that releases, and an allocation that is freed by the clock and not by
/// the host's confirmation.
#[tokio::test]
async fn the_next_attempt_waits_for_the_previous_allocation() {
    let limits = JobLimits {
        start_timeout: MINUTE,
        ..limits(DAY)
    };
    let mut rig = clocked_rig("retry_releasing", true, limits);
    rig.register("host-a", true);
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-t").await;
    rig.register("host-b", true);
    let mut b = FakeAgent::new("host-b");

    // The host holds on to the starting job: it beats, and ignores the stop it is sent.
    pass_time(&rig, MINUTE - SECOND, &mut [&mut b], &[&a]).await;
    assert_eq!(rig.job(&id).state, JobState::Starting);
    pass_time(&rig, SECOND, &mut [&mut b], &[&a]).await;
    let queued = rig.job(&id);
    assert_eq!(
        queued.state,
        JobState::Queued,
        "the start timed out and retries"
    );
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    let ended = queued
        .attempt
        .history()
        .last()
        .map(|attempt| attempt.ended_at);
    assert_eq!(
        queued.attempt.retry_not_before(),
        ended.map(|ended| ended.saturating_add(BACKOFF))
    );

    // The backoff passes; host-b is idle and alive; the previous allocation still holds.
    pass_time(&rig, BACKOFF + 10 * SECOND, &mut [&mut b], &[&a]).await;
    assert_eq!(rig.job(&id).state, JobState::Queued);
    let held = rig.allocation(&id);
    assert_eq!(
        (held.host.as_str(), held.step),
        ("host-a", AllocationStep::Releasing)
    );

    // The agent takes the stop, reports the job ended, and drops it once acknowledged.
    beat(&rig, &mut a).await;
    beat(&rig, &mut a).await;
    assert!(!a.holds("job-t"));
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    let reply = rig.beat_unpolled(&a, None).await.unwrap();
    a.take(&reply);
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::StartTimeout))
    );
    assert_eq!(rig.job(&id).state, JobState::Queued);

    rig.tick();
    rig.tick();
    let placed = rig.job(&id);
    assert_eq!(placed.state, JobState::Placed);
    assert_eq!(placed.attempt.number(), 2);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
}

/// Asserts that a host that goes silent while it holds the previous attempt's allocation
/// holds the retry too: the allocation stays releasing, an idle host is not given the job
/// after its backoff, and the job ends `FAILED_TO_START` `NO_CAPACITY` when its queue wait,
/// counted from its backoff's end, runs out (one millisecond before, it is still queued).
/// Catches a retry placed beside an allocation that is not freed, a silent host whose
/// allocation is freed by the clock, and a queue wait that does not end such a job.
#[tokio::test]
async fn a_silent_hosts_attempt_holds_its_retry_until_the_queue_wait() {
    let queue_wait = 5 * MINUTE;
    let limits = JobLimits {
        start_timeout: MINUTE,
        ..limits(queue_wait)
    };
    let mut rig = clocked_rig("retry_silent", true, limits);
    rig.register("host-a", true);
    let mut a = FakeAgent::new("host-a");
    let id = start(&rig, &mut a, "job-s").await;
    rig.register("host-b", true);
    let mut b = FakeAgent::new("host-b");

    // host-a never beats again: its start times out, and the retry is queued.
    pass_time(&rig, MINUTE, &mut [&mut b], &[]).await;
    assert_eq!(rig.job(&id).state, JobState::Queued);
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    let queued = rig.job(&id);
    let ended = queued
        .attempt
        .history()
        .last()
        .map(|attempt| attempt.ended_at);
    assert_eq!(
        queued.attempt.retry_not_before(),
        ended.map(|ended| ended.saturating_add(BACKOFF))
    );
    let wait_ends = not_before_ms(&rig, &id) + 300_000;

    pass_time(&rig, BACKOFF + SECOND, &mut [&mut b], &[]).await;
    assert_eq!(rig.job(&id).state, JobState::Queued, "past its backoff");
    pass_time(&rig, queue_wait - 2 * SECOND, &mut [&mut b], &[]).await;
    advance_to(&rig, wait_ends - 1);
    rig.tick();
    assert_eq!(
        rig.job(&id).state,
        JobState::Queued,
        "1 ms before its wait ends"
    );
    let held = rig.allocation(&id);
    assert_eq!(
        (held.host.as_str(), held.step),
        ("host-a", AllocationStep::Releasing)
    );
    assert_eq!(rig.job(&id).attempt.number(), 1);

    advance_to(&rig, wait_ends);
    rig.tick();
    let ended = JobState::FailedToStart(StartFailure::NoCapacity);
    assert_eq!(rig.job(&id).state, ended);
    let shown = view::job(&rig.job(&id));
    assert_eq!(
        shown.start_failure,
        i32::from(v1::StartFailureCode::NoCapacity)
    );
    assert_eq!(
        rig.allocation(&id).step,
        AllocationStep::Releasing,
        "no clock frees the silent host's allocation"
    );
}
