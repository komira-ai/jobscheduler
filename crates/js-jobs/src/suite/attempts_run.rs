//! The attempts of a job through its life: a job without a policy, a resubmit, what a retry
//! writes and stops, how a placement counts attempts, and how a cancel and a second retry meet
//! a retry.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::Store;
use js_state::conformance::Backend;
use js_types::{JobEventKind, QueueCode, QueueNote, RetryPolicy};

use super::{
    allocation_of, job_of, messages_of, now_of, place_and_run, refused_as, running_spec, submit,
    timed_out,
};
use crate::suite::whole::{engine_placement, history, later, placed_whole, queue_of, requeue_with};
use crate::suite::{
    Rig, attempt_end, exited, first_ended, full_spec, host, job_id, policy, rig, share,
};
use crate::{
    AllocationRecord, AllocationStep, AttemptOutcome, AttemptRecord, AttemptRow, Booked,
    DispatchKind, EndReason, Exclusion, JobFiles, JobRecord, JobSpec, JobState, JobStoreError,
    RETRY_HOST_BAR, RequeuedFor, ResultReason, StartFailure,
};

/// A job with no retry policy that is placed, runs and exits has one attempt: a retry of it is
/// refused, its end is the end a job has always had (its state and result), its attempt number
/// is 1, its history is the one attempt with its host and times, and its attempt row holds that
/// host and those times, with no row for a second attempt. Catches a job without a policy that
/// is retried, an end that does not append the attempt it ended, and an attempt row not written
/// with the end.
pub fn job_without_a_policy_ends_after_one_attempt<B: Backend>(backend: &B) {
    let name = "job_without_a_policy_ends_after_one_attempt";
    let rig = rig(backend);
    let spec = JobSpec {
        retry: None,
        ..full_spec()
    };
    let id = running_spec(&rig, "np-job", spec);
    let before = job_of(&rig, &id);
    let snapshot = rig.snapshot();
    assert_eq!(
        rig.jobs.retry_txn(&id, timed_out(), now_of(&rig)),
        Err(JobStoreError::IllegalTransition {
            id: id.clone(),
            from: JobState::Running,
            to: JobState::Queued,
        }),
        "{name}: a retry of a job without a policy"
    );
    assert_eq!(rig.snapshot(), snapshot, "{name}: the refusal wrote");
    let ended = rig.jobs.end_job(&id, exited(0)).expect("the exit ends");
    let at = ended.ended_at.expect("an ended job has an end time");
    assert_eq!(ended.state, JobState::Succeeded, "{name}: the state");
    assert_eq!(
        ended
            .result
            .as_ref()
            .map(|result| (result.exit_code, result.reason)),
        Some((Some(0), ResultReason::Exit)),
        "{name}: the result"
    );
    assert_eq!(ended.attempt.number(), 1, "{name}: the attempt number");
    let end = attempt_end(
        AttemptOutcome::Succeeded,
        Some(0),
        Some(ResultReason::Exit),
        None,
    );
    assert_eq!(
        ended.attempt,
        first_ended(&before, at, end, false),
        "{name}: the history is not the one attempt"
    );
    assert_eq!(job_of(&rig, &id), ended, "{name}: the stored job");
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Ok(Some(AttemptRow {
            host: host("host-a"),
            placed_at: before.placed_at.expect("a placed job"),
            ended_at: at,
        })),
        "{name}: the first attempt's row"
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 2),
        Ok(None),
        "{name}: a second attempt's row"
    );
    assert!(
        matches!(
            rig.jobs.retry_txn(&id, timed_out(), at),
            Err(JobStoreError::IllegalTransition {
                from: JobState::Succeeded,
                ..
            })
        ),
        "{name}: a retry of an ended job"
    );
}

/// A job submitted again under its id with the same retry policy is the same job (not
/// created); another attempt count, another first or longest wait, an absent wait, no policy
/// where there was one and a policy where there was none are an id conflict that writes
/// nothing, also once the job ended and its files are digests. The callback half of the
/// comparison belongs to the callback task and is not checked here. Catches a spec comparison
/// that leaves the policy out, or reads it as a priority is read.
pub fn a_resubmit_with_another_policy_conflicts<B: Backend>(backend: &B) {
    let name = "a_resubmit_with_another_policy_conflicts";
    let rig = rig(backend);
    let id = job_id("rs-job");
    assert_eq!(submit(&rig, &id, full_spec()), Ok(true), "{name}: first");
    let secs = |secs| Some(Duration::from_secs(secs));
    let others = [
        ("a larger count", Some(policy(4))),
        ("a smaller count", Some(policy(2))),
        (
            "another first wait",
            Some(RetryPolicy {
                initial_backoff: secs(6),
                ..policy(3)
            }),
        ),
        (
            "another longest wait",
            Some(RetryPolicy {
                max_backoff: secs(61),
                ..policy(3)
            }),
        ),
        (
            "an absent first wait",
            Some(RetryPolicy {
                initial_backoff: None,
                ..policy(3)
            }),
        ),
        (
            "an absent longest wait",
            Some(RetryPolicy {
                max_backoff: None,
                ..policy(3)
            }),
        ),
        ("no policy", None),
    ];
    for (what, retry) in others {
        let before = rig.snapshot();
        let spec = JobSpec {
            retry,
            ..full_spec()
        };
        assert_eq!(
            submit(&rig, &id, spec),
            Err(JobStoreError::JobIdConflict { id: id.clone() }),
            "{name}: {what}"
        );
        assert_eq!(rig.snapshot(), before, "{name}: {what} wrote");
    }
    assert_eq!(
        submit(&rig, &id, full_spec()),
        Ok(false),
        "{name}: the same policy"
    );

    let bare = job_id("rs-none");
    let none = JobSpec {
        retry: None,
        ..full_spec()
    };
    assert_eq!(submit(&rig, &bare, none.clone()), Ok(true), "{name}: none");
    assert_eq!(
        submit(&rig, &bare, none),
        Ok(false),
        "{name}: the same absence of a policy"
    );
    assert_eq!(
        submit(&rig, &bare, full_spec()),
        Err(JobStoreError::JobIdConflict { id: bare }),
        "{name}: a policy where there was none"
    );

    let done = running_spec(&rig, "rs-ended", full_spec());
    rig.jobs.end_job(&done, exited(0)).expect("the exit ends");
    assert!(
        matches!(job_of(&rig, &done).spec.files, JobFiles::Digests(_)),
        "{name}: the ended job keeps its files' contents"
    );
    assert_eq!(
        submit(&rig, &done, full_spec()),
        Ok(false),
        "{name}: the same policy after the end"
    );
    for (what, retry) in [("another count", Some(policy(4))), ("no policy", None)] {
        let spec = JobSpec {
            retry,
            ..full_spec()
        };
        assert_eq!(
            submit(&rig, &done, spec),
            Err(JobStoreError::JobIdConflict { id: done.clone() }),
            "{name}: {what} after the end"
        );
    }
}

/// A retry of a running whole-host job that was refused and requeued before (a failed prepare,
/// a held and a capacity refusal) is one commit that answers, and stores, the job as queued
/// with no host and no placement or start time, no result, its failed prepares and refusals
/// none, its host's bar of an hour kept (it outlasts the retry's own) and its other bars
/// kept, its next placement not before the time given, its attempt
/// number the same, the attempt in its history as retried and its row written, its place in the
/// queue and every index right and its allocation releasing. Catches a retry that keeps the
/// refusal counts of the attempt before, shortens or drops a bar, or leaves the job out of the
/// queue index.
pub fn a_retry_queues_the_next_attempt_with_its_counts_reset_and_its_host_barred<B: Backend>(
    backend: &B,
) {
    let name = "a_retry_queues_the_next_attempt_with_its_counts_reset_and_its_host_barred";
    let rig = rig(backend);
    let (id, _, _) = placed_whole(&rig, "rt-whole");
    let place = |rig: &Rig<_>| {
        rig.tick();
        rig.jobs
            .place_with(&id, host("host-a"), Booked::WholeHost, engine_placement())
            .expect("the job is placed");
    };
    let start = |rig: &Rig<_>| {
        rig.jobs
            .transition(&id, JobState::Placed, JobState::Starting)
            .expect("placed to starting");
    };
    rig.tick();
    rig.jobs
        .fail_prepare_with(&id, requeue_with(&rig, 1), &history)
        .expect("a failed prepare requeues");
    place(&rig);
    start(&rig);
    rig.jobs
        .requeue_held(&id, refused_as(&rig, RequeuedFor::Held), &history)
        .expect("a held refusal requeues");
    place(&rig);
    start(&rig);
    rig.jobs
        .requeue_held(&id, refused_as(&rig, RequeuedFor::Capacity), &history)
        .expect("a capacity refusal requeues");
    place(&rig);
    start(&rig);
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("starting to running");
    rig.tick();
    let after = rig.queue("rt-after");
    rig.tick();

    let before = job_of(&rig, &id);
    assert_eq!(
        (
            before.failed_prepares,
            before.held_refusals.clone(),
            before.capacity_refusals.clone()
        ),
        (1, vec![host("host-a")], vec![host("host-a")]),
        "{name}: the job holds no counts to reset"
    );
    let allocation = allocation_of(&rig, &id);
    let now = now_of(&rig);
    let not_before = later(&rig, 30);
    let end = attempt_end(
        AttemptOutcome::Failed,
        Some(1),
        Some(ResultReason::Exit),
        None,
    );
    let commits = rig.store.commit_stats().txns;
    let queued = rig
        .jobs
        .retry_txn(&id, end.clone(), not_before)
        .expect("a running job with attempts left is retried");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the retry took other than one commit"
    );
    let expected = JobRecord {
        state: JobState::Queued,
        host: None,
        placed_at: None,
        started_at: None,
        result: None,
        start_failure_detail: None,
        failed_prepares: 0,
        prepare_bars: BTreeMap::from([
            // The capacity refusal's bar of an hour outlasts the retry's.
            (host("host-a"), before.prepare_bars[&host("host-a")]),
            (host("host-x"), Exclusion::ForLife),
        ]),
        held_refusals: Vec::new(),
        capacity_refusals: Vec::new(),
        attempt: first_ended(&before, now, end, true).waiting(not_before),
        queue: {
            let mut queue = before.queue.clone();
            let note = QueueNote {
                code: QueueCode::WaitingForRetry,
                detail: "attempt 1 on host host-a ended: it ended; attempt 2 may be placed in 30 s"
                    .to_owned(),
                event: Some(JobEventKind::RetryScheduled),
                host: Some(host("host-a")),
                attempt: false,
            };
            queue.apply(&note, now);
            queue
        },
        ..before.clone()
    };
    assert_eq!(queued, expected, "{name}: the job answered");
    assert_eq!(job_of(&rig, &id), expected, "{name}: the job stored");
    assert_eq!(
        queued.attempt.retry_not_before(),
        Some(not_before),
        "{name}: the next placement's time"
    );
    assert_eq!(queued.attempt.number(), 1, "{name}: the attempt number");
    assert_eq!(
        queued
            .attempt
            .history()
            .last()
            .map(|attempt| attempt.retried),
        Some(true),
        "{name}: the attempt is not retried"
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Ok(Some(AttemptRow {
            host: host("host-a"),
            placed_at: before.placed_at.expect("a placed job"),
            ended_at: now,
        })),
        "{name}: the attempt's row"
    );
    assert_eq!(
        queue_of(&rig),
        [id.clone(), after],
        "{name}: the job is not in the queue at its place"
    );
    assert_eq!(
        allocation_of(&rig, &id),
        AllocationRecord {
            step: AllocationStep::Releasing,
            ..allocation
        },
        "{name}: the allocation"
    );
    assert_eq!(
        rig.jobs.index_entries(),
        rig.jobs.index_by_scan(),
        "{name}: the indexes differ from a scan"
    );
}

/// A retried job keeps the contents of its files (it has not ended, so they are not digests),
/// has no retention end, no end time, no result and no entry in the due index, and its
/// callback is not armed or answered: it reads as it did (the callback half belongs to the
/// callback task, so only that none is reported is checked). When its next attempt ends the
/// files become digests and the job is due. Catches a retry that digests the files the next
/// attempt needs, or makes the job due for the sweep.
pub fn a_retry_keeps_files_and_sets_no_retention_end<B: Backend>(backend: &B) {
    let name = "a_retry_keeps_files_and_sets_no_retention_end";
    let rig = rig(backend);
    let id = running_spec(&rig, "kf-job", full_spec());
    let files = job_of(&rig, &id).spec.files;
    assert!(matches!(files, JobFiles::Contents(_)), "{name}: the setup");
    let queued = rig
        .jobs
        .retry_txn(&id, timed_out(), later(&rig, 30))
        .expect("the retry");
    assert_eq!(queued.spec.files, files, "{name}: the files' contents");
    assert_eq!(
        (
            queued.retention_ends,
            queued.ended_at,
            queued.result.clone()
        ),
        (None, None, None),
        "{name}: the job ended"
    );
    assert_eq!(
        rig.store.scan_prefix(crate::keys::DUE).expect("scans"),
        [],
        "{name}: a due entry"
    );
    assert_eq!(
        rig.jobs.callback_status(&id),
        Ok(None),
        "{name}: the callback"
    );
    assert_eq!(job_of(&rig, &id), queued, "{name}: the stored job");

    rig.jobs
        .free(&id, EndReason::StartTimeout)
        .expect("frees the allocation");
    place_and_run(&rig, &id);
    let ended = rig.jobs.end_job(&id, exited(0)).expect("the exit ends");
    assert!(
        matches!(ended.spec.files, JobFiles::Digests(_)),
        "{name}: the files of the ended job"
    );
    assert_eq!(
        rig.store
            .scan_prefix(crate::keys::DUE)
            .expect("scans")
            .len(),
        1,
        "{name}: the ended job is not due"
    );
}

/// A retry leaves, in the host's dispatch queue, a stop for the attempt's start only where the
/// server decided the end and the start may have reached the host (a start timeout or a lost
/// host of a start handed out or a job started), none for a timeout of a start never handed out,
/// none for an end the host told (an agent's refusal, an input it could not read, a failure),
/// and in every case the start message is deleted. Catches a retry that leaves a start for the
/// host to run behind the next attempt, that stops a start a host refused, or that forgets the
/// stop of a start the host has.
pub fn a_retry_stops_its_start_only_where_the_server_decided_its_end<B: Backend>(backend: &B) {
    let name = "a_retry_stops_its_start_only_where_the_server_decided_its_end";
    let rig = rig(backend);
    let lost = attempt_end(AttemptOutcome::HostLost, None, None, None);
    let refused = |failure| AttemptRecord {
        start_failure: Some(failure),
        start_failure_detail: Some("refused".to_owned()),
        ..timed_out()
    };
    let failed = attempt_end(
        AttemptOutcome::Failed,
        Some(1),
        Some(ResultReason::Exit),
        None,
    );
    let stop = vec![(DispatchKind::Stop, false)];
    // Name, how the attempt ended, whether it ran, whether its start was handed out, its stop.
    let cases = [
        ("sx-timeout-sent", timed_out(), false, true, true),
        ("sx-timeout-unsent", timed_out(), false, false, false),
        ("sx-lost-running", lost.clone(), true, false, true),
        ("sx-lost-sent", lost, false, true, true),
        (
            "sx-refused",
            refused(StartFailure::AgentRefused),
            true,
            true,
            false,
        ),
        (
            "sx-input",
            refused(StartFailure::InputUnavailable),
            true,
            true,
            false,
        ),
        ("sx-failed", failed, true, true, false),
    ];
    for (text, end, ran, sent, stopped) in cases {
        let id = rig.queue(text);
        if ran {
            place_and_run(&rig, &id);
        } else {
            rig.jobs
                .place(&id, host("host-a"), share())
                .expect("placed");
        }
        if sent {
            rig.jobs
                .claim_dispatch(&host("host-a"), "server-1", Duration::from_secs(10))
                .expect("hands the start out");
        }
        assert_eq!(
            messages_of(&rig, &id).len(),
            1,
            "{name}: {text} holds no start"
        );
        rig.jobs
            .retry_txn(&id, end, later(&rig, 30))
            .expect("the retry");
        let expected = if stopped { stop.clone() } else { Vec::new() };
        assert_eq!(
            messages_of(&rig, &id),
            expected,
            "{name}: {text}: the start is left or the stop is wrong"
        );
        assert_eq!(
            allocation_of(&rig, &id).step,
            AllocationStep::Releasing,
            "{name}: {text}: the allocation"
        );
    }
}

/// A job's first placement is attempt 1, and so is a placement after a requeue before its run
/// (a failed whole-host prepare, a held refusal); after a retry the placement is refused while
/// the allocation of the attempt before is not freed (one attempt at a time) and, once it is
/// freed, counts attempt 2, a requeue within it keeps 2, and a second retry and placement count
/// 3, every attempt that ended keeping its row and the current one having none. Catches a
/// requeue that counts as an attempt, a placement after a retry that does not, and two
/// attempts of one job at once.
pub fn a_placement_after_a_retry_counts_the_next_attempt_and_a_requeue_does_not<B: Backend>(
    backend: &B,
) {
    let name = "a_placement_after_a_retry_counts_the_next_attempt_and_a_requeue_does_not";
    let rig = rig(backend);
    let a = host("host-a");
    let id = rig.queue("pc-job");
    let number = |rig: &Rig<_>| job_of(rig, &id).attempt.number();
    let (placed, _) = rig.jobs.place(&id, a.clone(), share()).expect("placed");
    assert_eq!(placed.attempt.number(), 1, "{name}: the first placement");
    rig.jobs
        .requeue_held(&id, refused_as(&rig, RequeuedFor::Held), &history)
        .expect("a held refusal requeues");
    assert_eq!(number(&rig), 1, "{name}: after a held refusal");
    let (placed, _) = rig.jobs.place(&id, a.clone(), share()).expect("placed");
    assert_eq!(placed.attempt.number(), 1, "{name}: after the requeue");

    let (whole, _, _) = placed_whole(&rig, "pc-whole");
    rig.jobs
        .requeue_failed_prepare(&whole)
        .expect("a failed prepare requeues");
    let (placed, _) = rig
        .jobs
        .place_with(&whole, a.clone(), Booked::WholeHost, engine_placement())
        .expect("placed");
    assert_eq!(
        placed.attempt.number(),
        1,
        "{name}: a whole-host job after a failed prepare"
    );

    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("placed to starting");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("starting to running");
    rig.tick();
    let queued = rig
        .jobs
        .retry_txn(&id, timed_out(), later(&rig, 30))
        .expect("the first retry");
    assert_eq!(queued.attempt.number(), 1, "{name}: waiting after a retry");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.place(&id, a.clone(), share()).map(drop),
        Err(JobStoreError::AllocationHeld {
            id: id.clone(),
            step: AllocationStep::Releasing,
        }),
        "{name}: a placement over the allocation of the attempt before"
    );
    assert_eq!(rig.snapshot(), before, "{name}: the refusal wrote");
    rig.jobs
        .free(&id, EndReason::StartTimeout)
        .expect("frees the allocation");
    let (placed, _) = rig.jobs.place(&id, a.clone(), share()).expect("placed");
    assert_eq!(placed.attempt.number(), 2, "{name}: after a retry");
    assert_eq!(job_of(&rig, &id), placed, "{name}: the stored job");

    rig.jobs
        .requeue_held(&id, refused_as(&rig, RequeuedFor::Capacity), &history)
        .expect("a capacity refusal requeues");
    let (placed, _) = rig.jobs.place(&id, a.clone(), share()).expect("placed");
    assert_eq!(placed.attempt.number(), 2, "{name}: a requeue in attempt 2");

    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("placed to starting");
    rig.jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("starting to running");
    rig.tick();
    rig.jobs
        .retry_txn(&id, timed_out(), later(&rig, 30))
        .expect("the second retry");
    rig.jobs
        .free(&id, EndReason::StartTimeout)
        .expect("frees the allocation");
    let (placed, _) = rig.jobs.place(&id, a.clone(), share()).expect("placed");
    assert_eq!(placed.attempt.number(), 3, "{name}: after a second retry");
    let rows = [1, 2, 3, 4].map(|n| {
        rig.jobs
            .attempt_row(&id, n)
            .expect("reads")
            .map(|row| row.host)
    });
    assert_eq!(
        rows,
        [Some(a.clone()), Some(a), None, None],
        "{name}: the attempt rows"
    );
}

/// A cancel of a retried job, which waits queued for its next attempt, ends it cancelled at
/// once in one commit, with a result and a retention end; no attempt is counted for it: its
/// number is the one that ended, its history keeps only the attempt that ran, and no row is
/// written for the next number. The callback half belongs to the callback task, so only that
/// none is reported is checked. Catches a cancel in backoff that counts or records an attempt
/// that never ran, or that waits for a host that holds nothing.
pub fn cancel_in_backoff_ends_the_job_at_once_without_a_new_attempt<B: Backend>(backend: &B) {
    let name = "cancel_in_backoff_ends_the_job_at_once_without_a_new_attempt";
    let rig = rig(backend);
    let id = running_spec(&rig, "cb-job", full_spec());
    let queued = rig
        .jobs
        .retry_txn(&id, timed_out(), later(&rig, 30))
        .expect("the retry");
    rig.tick();
    let retried_at = queued.attempt.history()[0].ended_at;
    assert_eq!(
        queued.prepare_bars,
        BTreeMap::from([(
            host("host-a"),
            Exclusion::Until(retried_at.saturating_add(RETRY_HOST_BAR))
        )]),
        "{name}: a host with no bar is barred from the retry for the retry's bar"
    );
    let now = now_of(&rig);
    let commits = rig.store.commit_stats().txns;
    let ended = rig.jobs.request_cancel(&id).expect("the cancel");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the cancel took other than one commit"
    );
    assert_eq!(ended.state, JobState::Cancelled, "{name}: the state");
    assert_eq!(ended.ended_at, Some(now), "{name}: the end time");
    assert_eq!(
        ended.result.as_ref().map(|result| result.reason),
        Some(ResultReason::Cancelled),
        "{name}: the result"
    );
    assert_eq!(
        ended.retention_ends,
        Some(now.saturating_add(rig.jobs.retention().window())),
        "{name}: the retention end"
    );
    assert_eq!(
        ended.attempt, queued.attempt,
        "{name}: an attempt was counted or recorded"
    );
    assert_eq!(ended.attempt.number(), 1, "{name}: the number");
    assert_eq!(ended.attempt.history().len(), 1, "{name}: the history");
    assert_eq!(job_of(&rig, &id), ended, "{name}: the stored job");
    assert_eq!(
        rig.raw(&crate::keys::attempt(&id, 2)),
        None,
        "{name}: a row for an attempt that never ran"
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 2),
        Ok(None),
        "{name}: the next attempt's row"
    );
    assert!(
        rig.jobs.attempt_row(&id, 1).expect("reads").is_some(),
        "{name}: the attempt that ran lost its row"
    );
    assert_eq!(queue_of(&rig), [], "{name}: the job is still queued");
    assert_eq!(
        rig.store
            .scan_prefix(crate::keys::DUE)
            .expect("scans")
            .len(),
        1,
        "{name}: the job is not due for the sweep"
    );
    assert_eq!(rig.jobs.callback_status(&id), Ok(None), "{name}: callback");
}

/// A cancel and a retry decided on one version of a running job make one change. The retry
/// that loses to a cancel is stale: the job keeps its cancel intent, is not queued and has no
/// attempt in its history. The cancel that loses to a retry is stale, the job stays retried and
/// queued without an intent, and the cancel asked again meets a queued job and ends it
/// cancelled at once, its history still the one attempt. Catches a retry that overrides a
/// cancel, a cancel lost behind a retry, and either write made on a row that moved.
pub fn a_cancel_and_a_retry_on_one_version_make_one_change<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_cancel_and_a_retry_on_one_version_make_one_change";
    let rig = rig(backend);

    let id = running_spec(&rig, "cr-cancel-first", full_spec());
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other.request_cancel(&target).expect("the other cancels");
    });
    assert_eq!(
        rig.jobs
            .retry_txn(&id, timed_out(), later(&rig, 30))
            .map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a retry over a cancel"
    );
    let job = job_of(&rig, &id);
    assert_eq!(
        (job.state, job.cancel_requested, job.attempt.history().len()),
        (JobState::Running, true, 0),
        "{name}: the cancel's intent or the retry's write"
    );
    assert_eq!(queue_of(&rig), [], "{name}: the job was queued");
    assert_eq!(
        allocation_of(&rig, &id).step,
        AllocationStep::Booked,
        "{name}: the allocation was released"
    );
    assert_eq!(
        rig.jobs.attempt_row(&id, 1),
        Ok(None),
        "{name}: the retry wrote a row"
    );

    let id = running_spec(&rig, "cr-retry-first", full_spec());
    let other = rig.other();
    let target = id.clone();
    let not_before = later(&rig, 30);
    rig.store.before_next(move || {
        other
            .retry_txn(&target, timed_out(), not_before)
            .expect("the other retries");
    });
    assert_eq!(
        rig.jobs.request_cancel(&id).map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a cancel over a retry"
    );
    let job = job_of(&rig, &id);
    assert_eq!(
        (
            job.state,
            job.cancel_requested,
            job.attempt.history().len(),
            job.attempt.retry_not_before()
        ),
        (JobState::Queued, false, 1, Some(not_before)),
        "{name}: the retry or the lost cancel"
    );
    let ended = rig.jobs.request_cancel(&id).expect("the cancel again");
    assert_eq!(
        ended.state,
        JobState::Cancelled,
        "{name}: the second cancel"
    );
    assert_eq!(
        ended.attempt, job.attempt,
        "{name}: the cancel of a queued job changed the attempts"
    );
    assert_eq!(job_of(&rig, &id), ended, "{name}: the stored job");
    assert_eq!(queue_of(&rig), [], "{name}: the cancelled job is queued");
}

/// Two retries decided on one version of a running job make one attempt: the one that commits
/// first stands, the other is stale, and the job has one entry in its history (the winner's),
/// one attempt row, and the winner's time for its next placement. Catches a retry that writes
/// over the winner's, one that appends its attempt twice, and a row written for a retry that
/// lost.
pub fn two_retry_decisions_on_one_version_make_one_attempt<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "two_retry_decisions_on_one_version_make_one_attempt";
    let rig = rig(backend);
    let id = running_spec(&rig, "tr-job", full_spec());
    let before = job_of(&rig, &id);
    let now = now_of(&rig);
    let winner_end = attempt_end(AttemptOutcome::HostLost, None, None, None);
    let winner_at = later(&rig, 10);
    let other = rig.other();
    let target = id.clone();
    let end = winner_end.clone();
    rig.store.before_next(move || {
        other
            .retry_txn(&target, end, winner_at)
            .expect("the other retries");
    });
    assert_eq!(
        rig.jobs
            .retry_txn(&id, timed_out(), later(&rig, 20))
            .map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: the second retry"
    );
    let job = job_of(&rig, &id);
    assert_eq!(
        job.attempt,
        first_ended(&before, now, winner_end, true).waiting(winner_at),
        "{name}: the attempt the job keeps"
    );
    let rows: Vec<String> = rig
        .store
        .scan_prefix("attempt/")
        .expect("scans")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        rows,
        [crate::keys::attempt(&id, 1)],
        "{name}: the attempt rows"
    );
    assert_eq!(
        rows,
        ["attempt/tr-job/0000000001".to_owned()],
        "{name}: the attempt row's key"
    );
}
