//! A job's attempts and its completion callback: what a retry is refused for, what it writes
//! when it is taken (the next attempt queued with its counts reset, its host barred, its files
//! kept, its start stopped where the server decided the end), how a placement counts attempts,
//! how a cancel and a second retry meet it; and the callback operations on jobs that ask for
//! none: a job without a callback has no callback status and none is due, and an answer to a
//! claim no row holds is stale, writing nothing.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError, StoreTime};
use js_types::{CallbackAnswer, FailureClass, SigningKey};

use super::whole::later;
use super::{Rig, full_spec, host, job_id, policy, rig, share};
use crate::callback::Held;
use crate::interfere::INJECTED;
use crate::{
    AllocationRecord, AttemptOutcome, AttemptRecord, CallbackClaim, DispatchKind, EndReason,
    Exclusion, HeldRequeue, InstanceId, JobId, JobRecord, JobSpec, JobState, JobStoreError, NewJob,
    RequeuedFor, StartFailure,
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

/// The full spec with a retry policy of `max` attempts.
fn retrying(max: u32) -> JobSpec {
    JobSpec {
        retry: Some(policy(max)),
        ..full_spec()
    }
}

/// Places the queued job `id` on `host-a` and moves it to running, a second apart each.
fn place_and_run<S: Store>(rig: &Rig<S>, id: &JobId) {
    rig.jobs
        .place(id, host("host-a"), share())
        .expect("a queued job is placed");
    rig.tick();
    rig.jobs
        .transition(id, JobState::Placed, JobState::Starting)
        .expect("placed to starting");
    rig.tick();
    rig.jobs
        .transition(id, JobState::Starting, JobState::Running)
        .expect("starting to running");
    rig.tick();
}

/// Records the job `text` with `spec`, places it and moves it to running.
fn running_spec<S: Store>(rig: &Rig<S>, text: &str, spec: JobSpec) -> JobId {
    let id = rig.queue_spec(text, spec);
    place_and_run(rig, &id);
    id
}

/// The job `id` as stored.
fn job_of<S: Store>(rig: &Rig<S>, id: &JobId) -> JobRecord {
    rig.jobs
        .get_job(id)
        .expect("reads")
        .expect("the job is recorded")
}

/// The allocation of the job `id` as stored.
fn allocation_of<S: Store>(rig: &Rig<S>, id: &JobId) -> AllocationRecord {
    rig.jobs
        .get_allocation(id)
        .expect("reads")
        .expect("the job has an allocation")
}

/// The store's clock now.
fn now_of<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.store.now().expect("the store's clock")
}

/// The messages queued on `host-a` for the job `id`: each one's kind and whether it was
/// handed out.
fn messages_of<S: Store>(rig: &Rig<S>, id: &JobId) -> Vec<(DispatchKind, bool)> {
    let queue = rig
        .jobs
        .dispatch_queue(&host("host-a"))
        .expect("reads the queue");
    queue
        .into_iter()
        .filter(|message| message.job == *id)
        .map(|message| (message.kind, message.claimed_by.is_some()))
        .collect()
}

/// A submit of the job `id` with `spec`: whether this call recorded it.
fn submit<S: Store>(rig: &Rig<S>, id: &JobId, spec: JobSpec) -> Result<bool, JobStoreError> {
    rig.jobs
        .insert_queued(NewJob {
            id: Some(id.clone()),
            spec,
            priority: 0,
            client: None,
        })
        .map(|inserted| inserted.created)
}

/// The requeue of a refused start the checks below make: `host-a` barred for an hour beside a
/// bar for life on `host-x`.
fn refused_as<S: Store>(rig: &Rig<S>, refused: RequeuedFor) -> HeldRequeue {
    HeldRequeue {
        refused,
        prepare_bars: BTreeMap::from([
            (host("host-a"), Exclusion::Until(later(rig, 3600))),
            (host("host-x"), Exclusion::ForLife),
        ]),
        queue: None,
    }
}

/// A retry of a job that is queued, or running with no policy, a policy of one or none, no
/// attempt left, or a cancel asked for, or that is cancelled, is refused as the move from its
/// state to queued; a retry of a job that does not exist is not found, and one whose read fails
/// is the store's error; every row and the commit counters read back as before each. A policy of
/// two at the first attempt, and the largest count, are accepted. Catches a retry that takes
/// a job its policy has no attempt left for, one that ignores a cancel, and a retry that writes
/// before it refuses.
pub fn a_retry_is_refused_as_an_illegal_move_and_writes_nothing<B: Backend>(backend: &B) {
    let name = "a_retry_is_refused_as_an_illegal_move_and_writes_nothing";
    let rig = rig(backend);
    let queued = rig.queue("ry-queued");
    let no_policy = JobSpec {
        retry: None,
        ..full_spec()
    };
    let running = running_spec(&rig, "ry-running", no_policy);
    let one = running_spec(&rig, "ry-one", retrying(1));
    let zero = running_spec(&rig, "ry-zero", retrying(0));
    let last = running_spec(&rig, "ry-last", retrying(2));
    let not_before = StoreTime::from_duration(Duration::from_secs(3600));
    let first = rig
        .jobs
        .retry_txn(&last, timed_out(), not_before)
        .expect("a retry at attempt 1 of 2");
    assert_eq!(first.state, JobState::Queued, "{name}: attempt 1 of 2");
    rig.jobs
        .free(&last, EndReason::StartTimeout)
        .expect("frees the first attempt's allocation");
    place_and_run(&rig, &last);
    assert_eq!(
        job_of(&rig, &last).attempt.number(),
        2,
        "{name}: the second attempt was not counted"
    );
    let asked = running_spec(&rig, "ry-asked", retrying(3));
    rig.jobs
        .request_cancel(&asked)
        .expect("a running job's cancel");
    let cancelled_job = rig.queue("ry-cancelled");
    rig.jobs
        .request_cancel(&cancelled_job)
        .expect("a queued job's cancel");
    rig.tick();
    let cases = [
        (&queued, JobState::Queued),
        (&running, JobState::Running),
        (&one, JobState::Running),
        (&zero, JobState::Running),
        (&last, JobState::Running),
        (&asked, JobState::Running),
        (&cancelled_job, JobState::Cancelled),
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
    let retryable = running_spec(&rig, "ry-retryable", retrying(3));
    let before = rig.snapshot();
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.retry_txn(&retryable, timed_out(), not_before),
        Err(injected()),
        "{name}: a failed read"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed read changed the store"
    );
    let largest = running_spec(&rig, "ry-largest", retrying(u32::MAX));
    let taken = rig
        .jobs
        .retry_txn(&largest, timed_out(), not_before)
        .expect("a policy of the largest count leaves an attempt");
    assert_eq!(
        (taken.state, taken.attempt.history().len()),
        (JobState::Queued, 1),
        "{name}: the largest count"
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
        held: Held {
            by: InstanceId::new("server-a"),
            tries: 1,
        },
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

#[path = "attempts_run.rs"]
mod run;
pub use run::*;
