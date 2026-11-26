//! A job's attempts and its completion callback before the store keeps either: a retry is
//! refused as the illegal move it is today, no job has an attempt row or a callback, none is
//! due, an answer is stale, and none of it writes anything.

use js_state::conformance::Backend;
use js_state::{StoreError, StoreTime, Version};
use js_types::{CallbackAnswer, FailureClass, SigningKey};

use super::{job_id, rig};
use crate::interfere::INJECTED;
use crate::{
    AttemptOutcome, AttemptRecord, CallbackClaim, InstanceId, JobState, JobStoreError, StartFailure,
};

/// The end of an attempt that failed to start for a cause of its host.
fn timed_out() -> AttemptRecord {
    AttemptRecord {
        outcome: AttemptOutcome::StartFailed,
        exit_code: None,
        reason: None,
        start_failure: Some(StartFailure::StartTimeout),
        start_failure_detail: Some("start-timeout".to_owned()),
    }
}

/// The error an operation answers when its read fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// A retry of a queued, a running and a cancelled job is refused as the move from its state
/// to queued, a retry of a job that does not exist as not found, and a retry whose read fails
/// as the store's error; every row and the commit counters read back as before each.
pub fn a_retry_is_refused_as_an_illegal_move_and_writes_nothing<B: Backend>(backend: &B) {
    let name = "a_retry_is_refused_as_an_illegal_move_and_writes_nothing";
    let rig = rig(backend);
    let queued = rig.queue("ry-queued");
    let running = rig.running("ry-running");
    let cancelled = rig.queue("ry-cancelled");
    rig.jobs
        .request_cancel(&cancelled)
        .expect("a queued job's cancel");
    rig.tick();
    let not_before = StoreTime::from_duration(std::time::Duration::from_secs(3600));
    let cases = [
        (&queued, JobState::Queued),
        (&running, JobState::Running),
        (&cancelled, JobState::Cancelled),
    ];
    for (id, from) in cases {
        let before = rig.snapshot();
        assert_eq!(
            rig.jobs.retry_txn(id, timed_out(), not_before),
            Err(JobStoreError::IllegalTransition {
                id: id.clone(),
                from,
                to: JobState::Queued,
            }),
            "{name}: {id}"
        );
        assert_eq!(rig.snapshot(), before, "{name}: {id} changed the store");
    }
    let absent = job_id("ry-absent");
    assert_eq!(
        rig.jobs.retry_txn(&absent, timed_out(), not_before),
        Err(JobStoreError::NotFound { id: absent }),
        "{name}: an absent job"
    );
    let before = rig.snapshot();
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.retry_txn(&running, timed_out(), not_before),
        Err(injected()),
        "{name}: a failed read"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed read changed the store"
    );
}

/// No job has an attempt row (its first attempt and the largest number alike) or a callback,
/// and no callback is due for any instance, however many it asks for; a job that does not
/// exist is not found and a failed read is the store's error; nothing is written.
pub fn no_job_has_an_attempt_row_or_a_callback_yet<B: Backend>(backend: &B) {
    let name = "no_job_has_an_attempt_row_or_a_callback_yet";
    let rig = rig(backend);
    let queued = rig.queue("ac-queued");
    let running = rig.running("ac-running");
    rig.tick();
    let before = rig.snapshot();
    for id in [&queued, &running] {
        for attempt in [0, 1, 2, u32::MAX] {
            assert_eq!(
                rig.jobs.attempt_row(id, attempt),
                Ok(None),
                "{name}: {id} attempt {attempt}"
            );
        }
        assert_eq!(rig.jobs.callback_status(id), Ok(None), "{name}: {id}");
    }
    let now = StoreTime::from_duration(std::time::Duration::from_secs(1 << 40));
    for limit in [0, 1, usize::MAX] {
        assert_eq!(
            rig.jobs
                .claim_callbacks(&InstanceId::new("server-a"), now, limit),
            Ok(Vec::new()),
            "{name}: a claim of {limit}"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a read changed the store");

    let absent = job_id("ac-absent");
    assert_eq!(
        rig.jobs.attempt_row(&absent, 1),
        Err(JobStoreError::NotFound { id: absent.clone() }),
        "{name}: an absent job's attempt"
    );
    assert_eq!(
        rig.jobs.callback_status(&absent),
        Err(JobStoreError::NotFound { id: absent }),
        "{name}: an absent job's callback"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.attempt_row(&running, 1),
        Err(injected()),
        "{name}: a failed read of an attempt"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.callback_status(&running),
        Err(injected()),
        "{name}: a failed read of a callback"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed read changed the store"
    );
}

/// An answer of each kind to a claim that no callback row holds (a delivery, an abandon and
/// each class of failure) is refused as stale for its job, and writes nothing.
pub fn an_answer_to_a_claim_no_row_holds_is_stale<B: Backend>(backend: &B) {
    let name = "an_answer_to_a_claim_no_row_holds_is_stale";
    let rig = rig(backend);
    let job = rig.queue("an-job");
    rig.tick();
    let claim = CallbackClaim {
        job: job.clone(),
        url: "https://hooks.example/js-done/ref-1".to_owned(),
        key: SigningKey::new(vec![b'k'; 32]).expect("a key"),
        webhook_id: "bXNnX2lk".to_owned(),
        version: Version(1),
    };
    let answers = [
        CallbackAnswer::Delivered,
        CallbackAnswer::Abandoned,
        CallbackAnswer::Failed(FailureClass::RefusedAddress),
        CallbackAnswer::Failed(FailureClass::Failed),
    ];
    let now = StoreTime::from_duration(std::time::Duration::from_secs(60));
    for answer in answers {
        let before = rig.snapshot();
        assert_eq!(
            rig.jobs.answer_callback(claim.clone(), answer, now),
            Err(JobStoreError::Stale { id: job.clone() }),
            "{name}: {answer:?}"
        );
        assert_eq!(
            rig.snapshot(),
            before,
            "{name}: {answer:?} changed the store"
        );
    }
}
