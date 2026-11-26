//! The retry and completion-callback scenarios of the job-service suite
//! (`docs/design/job-retry-and-completion-callback.md`, sections 3 and 5). Each reads the job
//! only through the client; the hooks lose a job's host, fail its start, deliver its callback
//! and move the clock, which is never real time.

use std::time::Duration;

use js_proto::v1::{self, AttemptOutcome, CallbackState, JobSpec, JobState};
use tonic::Code;

use super::scenarios::{Notes, TO_RUNNING, cancelled, drive, read, result_of, state_is, submitted};
use super::{JobHarness, Step, spec};

/// The longest wait of the policies the scenarios submit, in seconds. Advancing the clock by
/// it passes any wait the policy can give.
const MAX_WAIT: u64 = 60;

/// [`spec`] with a policy of `max_attempts` attempts, waiting 10 seconds before the second
/// and at most [`MAX_WAIT`] between any two.
fn retrying(id: &str, max_attempts: u32) -> JobSpec {
    JobSpec {
        retry: Some(v1::RetryPolicy {
            max_attempts,
            initial_backoff_seconds: Some(10),
            max_backoff_seconds: Some(u32::try_from(MAX_WAIT).unwrap_or(u32::MAX)),
        }),
        ..spec(id)
    }
}

/// The deadline the callback of `call-deadline` names, shorter than the retention window,
/// so the deadline the job shows is the job's end plus it.
const CALLBACK_DEADLINE_SECONDS: u32 = 20;

/// [`retrying`] with a completion callback: an https receiver and a 32-byte key.
fn calling(id: &str, max_attempts: u32) -> JobSpec {
    JobSpec {
        callback: Some(v1::Callback {
            url: "https://receiver.example/conformance".to_string(),
            signing_key: vec![7; 32],
            deadline_seconds: None,
        }),
        ..retrying(id, max_attempts)
    }
}

/// The ended attempts a job shows: number, outcome and whether another attempt followed.
fn history(job: &Option<v1::Job>) -> Vec<(u32, AttemptOutcome, bool)> {
    let attempts = job.iter().flat_map(|job| job.attempt_history.iter());
    let shown = attempts.map(|attempt| (attempt.number, attempt.outcome(), attempt.retried));
    shown.collect()
}

/// The attempt a job shows as its current one, 0 for a job that was not read.
fn attempt_of(job: &Option<v1::Job>) -> u32 {
    job.as_ref().map_or(0, |job| job.attempt)
}

/// The callback state and the tries a job shows; `None` for a job with no callback status.
fn callback_of(job: &Option<v1::Job>) -> Option<(CallbackState, u32)> {
    let status = job.as_ref().and_then(|job| job.callback_status.as_ref());
    status.map(|status| (status.state(), status.tries))
}

/// How long, in milliseconds, a job that waits for its retry waits: from the end of the
/// attempt its history ends with to the time `retry_not_before_ms` gives.
fn wait_ms(job: &Option<v1::Job>) -> Option<u64> {
    job.as_ref().and_then(|job| {
        let last = job.attempt_history.last();
        let ended = last
            .and_then(|attempt| attempt.times)
            .and_then(|times| times.ended_at_ms);
        let until = job.retry_not_before_ms;
        until
            .zip(ended)
            .and_then(|(until, ended)| until.checked_sub(ended))
    })
}

/// Whether `job` waits for its retry for a time within the policy: after the end of its last
/// attempt and by [`MAX_WAIT`] at the latest.
fn waits_within_policy(job: &Option<v1::Job>) -> bool {
    wait_ms(job).is_some_and(|wait| (1..=MAX_WAIT * 1000).contains(&wait))
}

/// Whether `job` shows the queue status of a job that waits for its retry.
fn waits_for_retry(job: &Option<v1::Job>) -> bool {
    let status = job.as_ref().and_then(|job| job.queue_status.as_ref());
    status.is_some_and(|status| status.code() == v1::QueueStatusCode::WaitingForRetry)
}

/// Loses the host of `id` and passes any wait its policy gives.
fn lose_and_wait<H: JobHarness>(harness: &mut H, notes: &mut Notes, id: &str) {
    drive(harness, notes, id, &[Step::HostLost]);
    harness.advance(Duration::from_secs(MAX_WAIT));
}

/// The host loss rows: a retried loss, one that runs out of attempts, and one without a
/// policy.
pub(super) async fn on_host_loss<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();

    submitted(&mut client, notes, "SubmitJob", retrying("lost-once", 3)).await;
    drive(harness, notes, "lost-once", &TO_RUNNING);
    drive(harness, notes, "lost-once", &[Step::HostLost]);
    let job = read(&mut client, notes, "lost-once").await;
    notes.expect(
        state_is(&job, JobState::Queued),
        "a job whose host is lost with attempts left is queued again",
    );
    notes.expect(
        attempt_of(&job) == 1,
        "the job still shows the attempt that ended",
    );
    let lost = vec![(1, AttemptOutcome::HostLost, true)];
    notes.expect(
        history(&job) == lost,
        &format!(
            "the lost attempt is in the history as retried: {:?}",
            history(&job)
        ),
    );
    notes.expect(
        waits_within_policy(&job),
        "the retry waits, for no longer than the policy's longest wait",
    );
    notes.expect(
        waits_for_retry(&job),
        "the queue status says it waits for its retry",
    );
    let held = job.as_ref().is_some_and(|job| job.host_id.is_empty());
    notes.expect(held, "the job holds no host while it waits");
    let asked = job.as_ref().is_some_and(|job| job.cancel_requested);
    notes.expect(!asked, "no cancel is shown");
    let answer = client
        .get_result(v1::GetResultRequest {
            job_id: "lost-once".to_string(),
        })
        .await;
    notes.refused(
        "GetResult of a job that waits for its retry",
        answer,
        (Code::FailedPrecondition, "not-finished"),
    );
    notes.expect(
        harness.step("lost-once", Step::HostLost).is_err(),
        "a host that is lost holds no job that waits for its retry",
    );
    harness.advance(Duration::from_secs(1));
    let early = harness.step("lost-once", Step::Place);
    notes.expect(
        early.is_err(),
        "the retry is not placed before its wait has passed",
    );
    harness.advance(Duration::from_secs(MAX_WAIT));
    drive(harness, notes, "lost-once", &TO_RUNNING);
    let job = read(&mut client, notes, "lost-once").await;
    notes.expect(
        state_is(&job, JobState::Running) & (attempt_of(&job) == 2),
        "the next attempt runs as attempt 2",
    );
    notes.expect(
        job.as_ref()
            .is_some_and(|job| job.retry_not_before_ms.is_none()),
        "a job past the queue shows no retry time",
    );
    drive(harness, notes, "lost-once", &[Step::Exit(0)]);
    let job = read(&mut client, notes, "lost-once").await;
    notes.expect(
        state_is(&job, JobState::Succeeded) & (attempt_of(&job) == 2),
        "the job succeeds on its second attempt",
    );
    let both = vec![
        (1, AttemptOutcome::HostLost, true),
        (2, AttemptOutcome::Succeeded, false),
    ];
    notes.expect(
        history(&job) == both,
        &format!("both attempts are in the history: {:?}", history(&job)),
    );
    let result = result_of(&mut client, notes, "lost-once").await;
    let shown = result.map(|result| (result.state(), result.attempt));
    notes.expect(
        shown == Some((JobState::Succeeded, 2)),
        "the result is the last attempt's",
    );

    submitted(&mut client, notes, "SubmitJob", retrying("lost-out", 2)).await;
    drive(harness, notes, "lost-out", &[Step::Place]);
    lose_and_wait(harness, notes, "lost-out");
    drive(harness, notes, "lost-out", &[Step::Place, Step::Start]);
    drive(harness, notes, "lost-out", &[Step::HostLost]);
    let job = read(&mut client, notes, "lost-out").await;
    notes.expect(
        state_is(&job, JobState::Lost) & (attempt_of(&job) == 2),
        "a job whose attempts ran out ends lost on its last",
    );
    let out = vec![
        (1, AttemptOutcome::HostLost, true),
        (2, AttemptOutcome::HostLost, false),
    ];
    notes.expect(
        history(&job) == out,
        &format!("the last attempt is not retried: {:?}", history(&job)),
    );
    notes.expect(
        job.as_ref()
            .is_some_and(|job| job.retry_not_before_ms.is_none()),
        "a job that ended shows no retry time",
    );
    let result = result_of(&mut client, notes, "lost-out").await;
    let shown = result.map(|result| (result.state(), result.attempt));
    notes.expect(
        shown == Some((JobState::Lost, 2)),
        "the result of a lost job names its state and its last attempt",
    );
    notes.expect(
        harness.step("lost-out", Step::HostLost).is_err(),
        "a job that ended is not lost again",
    );

    submitted(&mut client, notes, "SubmitJob", spec("lost-bare")).await;
    drive(harness, notes, "lost-bare", &TO_RUNNING);
    drive(harness, notes, "lost-bare", &[Step::HostLost]);
    let job = read(&mut client, notes, "lost-bare").await;
    notes.expect(
        state_is(&job, JobState::Lost) & (attempt_of(&job) == 1),
        "a job without a policy ends lost on its only attempt",
    );
    let bare = vec![(1, AttemptOutcome::HostLost, false)];
    notes.expect(
        history(&job) == bare,
        &format!("its attempt is not retried: {:?}", history(&job)),
    );
}

/// The start failure rows: a retried start that timed out, one that runs out of attempts, a
/// job without a policy, and a job whose start cannot fail any more.
pub(super) async fn on_start_failure<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();

    submitted(&mut client, notes, "SubmitJob", retrying("slow-once", 2)).await;
    drive(
        harness,
        notes,
        "slow-once",
        &[Step::Place, Step::StartFailed],
    );
    let job = read(&mut client, notes, "slow-once").await;
    notes.expect(
        state_is(&job, JobState::Queued) & (attempt_of(&job) == 1),
        "a job whose start timed out with attempts left is queued again",
    );
    let slow = vec![(1, AttemptOutcome::StartFailed, true)];
    notes.expect(
        history(&job) == slow,
        &format!(
            "the attempt is in the history as retried: {:?}",
            history(&job)
        ),
    );
    let code = job
        .as_ref()
        .and_then(|job| job.attempt_history.first())
        .map(|attempt| attempt.start_failure());
    notes.expect(
        code == Some(v1::StartFailureCode::StartTimeout),
        "the attempt names its start failure",
    );
    notes.expect(
        waits_within_policy(&job),
        "the retry waits within the policy",
    );
    harness.advance(Duration::from_secs(MAX_WAIT));
    drive(harness, notes, "slow-once", &[Step::Place, Step::Start]);
    drive(harness, notes, "slow-once", &[Step::StartFailed]);
    let job = read(&mut client, notes, "slow-once").await;
    notes.expect(
        state_is(&job, JobState::FailedToStart) & (attempt_of(&job) == 2),
        "a job whose attempts ran out ends failed to start on its last",
    );
    let shown = job.as_ref().map(|job| job.start_failure());
    notes.expect(
        shown == Some(v1::StartFailureCode::StartTimeout),
        "the job names its start failure",
    );
    let out = vec![
        (1, AttemptOutcome::StartFailed, true),
        (2, AttemptOutcome::StartFailed, false),
    ];
    notes.expect(
        history(&job) == out,
        &format!("the last attempt is not retried: {:?}", history(&job)),
    );
    let result = result_of(&mut client, notes, "slow-once").await;
    let shown = result.map(|result| (result.state(), result.attempt));
    notes.expect(
        shown == Some((JobState::FailedToStart, 2)),
        "the result names the state and the last attempt",
    );

    submitted(&mut client, notes, "SubmitJob", spec("slow-bare")).await;
    drive(
        harness,
        notes,
        "slow-bare",
        &[Step::Place, Step::StartFailed],
    );
    let job = read(&mut client, notes, "slow-bare").await;
    notes.expect(
        state_is(&job, JobState::FailedToStart) & (attempt_of(&job) == 1),
        "a job without a policy ends failed to start on its only attempt",
    );
    let bare = vec![(1, AttemptOutcome::StartFailed, false)];
    notes.expect(history(&job) == bare, "its attempt is not retried");

    submitted(&mut client, notes, "SubmitJob", retrying("slow-running", 2)).await;
    drive(harness, notes, "slow-running", &TO_RUNNING);
    notes.expect(
        harness.step("slow-running", Step::StartFailed).is_err(),
        "the start of a job that runs has not failed",
    );
    let job = read(&mut client, notes, "slow-running").await;
    notes.expect(
        state_is(&job, JobState::Running),
        "a refused move leaves the job as it was",
    );
}

/// A job that exits with a nonzero code ends failed, whatever its policy.
pub(super) async fn not_on_nonzero_exit<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    submitted(&mut client, notes, "SubmitJob", retrying("exits", 3)).await;
    drive(harness, notes, "exits", &TO_RUNNING);
    drive(harness, notes, "exits", &[Step::Exit(3)]);
    let job = read(&mut client, notes, "exits").await;
    notes.expect(
        state_is(&job, JobState::Failed) & (attempt_of(&job) == 1),
        "a nonzero exit ends the job failed, with attempts left",
    );
    let failed = vec![(1, AttemptOutcome::Failed, false)];
    notes.expect(
        history(&job) == failed,
        &format!("the attempt is not retried: {:?}", history(&job)),
    );
    let code = job
        .as_ref()
        .and_then(|job| job.attempt_history.first())
        .and_then(|attempt| attempt.exit_code);
    notes.expect(code == Some(3), "the attempt shows its exit code");
    notes.expect(
        job.as_ref()
            .is_some_and(|job| job.retry_not_before_ms.is_none()),
        "no retry time is shown",
    );
    let result = result_of(&mut client, notes, "exits").await;
    let shown = result.map(|result| (result.state(), result.attempt, result.exit_code));
    notes.expect(
        shown == Some((JobState::Failed, 1, Some(3))),
        "the result is the only attempt's",
    );
}

/// A job whose cancel was asked for is not retried when its host is lost.
pub(super) async fn not_on_cancel<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();
    submitted(&mut client, notes, "SubmitJob", retrying("asked", 3)).await;
    drive(harness, notes, "asked", &TO_RUNNING);
    let job = cancelled(&mut client, notes, "asked").await;
    let asked = job.as_ref().is_some_and(|job| job.cancel_requested);
    notes.expect(asked, "a cancel of a running job is only asked for");
    drive(harness, notes, "asked", &[Step::HostLost]);
    let job = read(&mut client, notes, "asked").await;
    notes.expect(
        state_is(&job, JobState::Lost) & (attempt_of(&job) == 1),
        "a cancel wins over a retry: the job ends lost on its first attempt",
    );
    let lost = vec![(1, AttemptOutcome::HostLost, false)];
    notes.expect(
        history(&job) == lost,
        &format!("the attempt is not retried: {:?}", history(&job)),
    );
    let asked = job.as_ref().is_some_and(|job| job.cancel_requested);
    notes.expect(
        asked,
        "a job that ended lost still shows the cancel asked for",
    );
}

/// The callback of a job with attempts: armed through every retry, due at the final end,
/// delivered once; a callback that names its deadline is due until the job's end plus it.
pub(super) async fn callback_once<H: JobHarness>(harness: &mut H, notes: &mut Notes) {
    let mut client = harness.client();

    let sent = submitted(&mut client, notes, "SubmitJob", calling("call-ok", 3)).await;
    notes.expect(
        callback_of(&sent) == Some((CallbackState::Armed, 0)),
        "a job with a callback is armed when it is submitted",
    );
    let key = sent
        .as_ref()
        .and_then(|job| job.spec.as_ref())
        .and_then(|spec| spec.callback.as_ref());
    notes.expect(
        key.is_some_and(|callback| callback.signing_key.is_empty() & !callback.url.is_empty()),
        "the job shows its callback's URL and never its key",
    );
    drive(harness, notes, "call-ok", &TO_RUNNING);
    lose_and_wait(harness, notes, "call-ok");
    let job = read(&mut client, notes, "call-ok").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Armed, 0)),
        "a retry does not make the callback due",
    );
    notes.expect(
        harness.step("call-ok", Step::DeliverCallback).is_err(),
        "no callback is delivered while the job waits for a retry",
    );
    drive(harness, notes, "call-ok", &TO_RUNNING);
    let job = read(&mut client, notes, "call-ok").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Armed, 0)),
        "the callback stays armed while the next attempt runs",
    );
    notes.expect(
        harness.step("call-ok", Step::DeliverCallback).is_err(),
        "no callback is delivered while the job runs",
    );
    drive(harness, notes, "call-ok", &[Step::Exit(0)]);
    let job = read(&mut client, notes, "call-ok").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Pending, 0)),
        "the callback is due when the job ends",
    );
    let deadline = job
        .as_ref()
        .and_then(|job| job.callback_status.as_ref())
        .and_then(|status| status.deadline_at_ms);
    notes.expect(
        deadline.is_some(),
        "a callback that is due has its deadline",
    );
    drive(harness, notes, "call-ok", &[Step::DeliverCallback]);
    let job = read(&mut client, notes, "call-ok").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Delivered, 1)),
        "the callback is delivered with one try",
    );
    let delivered = job
        .as_ref()
        .and_then(|job| job.callback_status.as_ref())
        .and_then(|status| status.delivered_at_ms);
    notes.expect(delivered.is_some(), "a delivered callback shows when");
    notes.expect(
        harness.step("call-ok", Step::DeliverCallback).is_err(),
        "a callback is delivered once",
    );

    submitted(&mut client, notes, "SubmitJob", calling("call-out", 2)).await;
    drive(harness, notes, "call-out", &TO_RUNNING);
    lose_and_wait(harness, notes, "call-out");
    drive(harness, notes, "call-out", &TO_RUNNING);
    let job = read(&mut client, notes, "call-out").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Armed, 0)),
        "the callback is armed on the last attempt",
    );
    drive(harness, notes, "call-out", &[Step::HostLost]);
    let job = read(&mut client, notes, "call-out").await;
    notes.expect(
        state_is(&job, JobState::Lost) & (callback_of(&job) == Some((CallbackState::Pending, 0))),
        "a job lost on its last attempt has its callback due",
    );
    drive(harness, notes, "call-out", &[Step::DeliverCallback]);
    let job = read(&mut client, notes, "call-out").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Delivered, 1)),
        "the callback of a lost job is delivered once",
    );

    submitted(&mut client, notes, "SubmitJob", calling("call-cancel", 3)).await;
    drive(harness, notes, "call-cancel", &TO_RUNNING);
    lose_and_wait(harness, notes, "call-cancel");
    let job = cancelled(&mut client, notes, "call-cancel").await;
    notes.expect(
        state_is(&job, JobState::Cancelled) & (attempt_of(&job) == 1),
        "a cancel ends a job that waits for its retry at once, on the attempt that ended",
    );
    let lost = vec![(1, AttemptOutcome::HostLost, true)];
    notes.expect(
        history(&job) == lost,
        &format!("no attempt follows the cancel: {:?}", history(&job)),
    );
    let job = read(&mut client, notes, "call-cancel").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Pending, 0)),
        "the callback of a cancelled job is due",
    );
    notes.expect(
        harness.step("call-cancel", Step::Place).is_err(),
        "a cancelled job is not placed again",
    );
    drive(harness, notes, "call-cancel", &[Step::DeliverCallback]);
    let job = read(&mut client, notes, "call-cancel").await;
    notes.expect(
        callback_of(&job) == Some((CallbackState::Delivered, 1)),
        "the callback of a cancelled job is delivered once",
    );

    let mut asked = calling("call-deadline", 1);
    if let Some(callback) = asked.callback.as_mut() {
        callback.deadline_seconds = Some(CALLBACK_DEADLINE_SECONDS);
    }
    submitted(&mut client, notes, "SubmitJob", asked).await;
    drive(harness, notes, "call-deadline", &TO_RUNNING);
    drive(harness, notes, "call-deadline", &[Step::Exit(0)]);
    let job = read(&mut client, notes, "call-deadline").await;
    let job = job.as_ref();
    let ended = job.and_then(|job| job.times.as_ref()?.ended_at_ms);
    let deadline = job.and_then(|job| job.callback_status.as_ref()?.deadline_at_ms);
    let asked_ms = u64::from(CALLBACK_DEADLINE_SECONDS) * 1000;
    notes.expect(
        ended.is_some() & (deadline == ended.map(|ended| ended + asked_ms)),
        &format!("a callback that names its deadline is due until the job's end plus it: ended {ended:?}, deadline {deadline:?}"),
    );

    submitted(&mut client, notes, "SubmitJob", spec("call-none")).await;
    drive(harness, notes, "call-none", &TO_RUNNING);
    drive(harness, notes, "call-none", &[Step::Exit(0)]);
    let job = read(&mut client, notes, "call-none").await;
    notes.expect(
        callback_of(&job).is_none(),
        "a job that asked for no callback shows no status",
    );
    notes.expect(
        harness.step("call-none", Step::DeliverCallback).is_err(),
        "a job that asked for no callback has none to deliver",
    );
}
