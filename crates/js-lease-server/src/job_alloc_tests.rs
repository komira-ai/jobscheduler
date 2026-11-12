//! The one way an allocation is freed, and the answer to a retired id.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Interfere;
use js_jobs::{
    AllocationStep, Booked, EndReason, JobEnd, JobId, JobState, JobStore, JobStoreError, NewJob,
    Output, ResultReason, StartFailure, keys,
};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Store, Txn};
use js_types::HostId;
use js_usage::{
    AllocationId, Booked as UsageBooked, DrainOutcome, EndedHistory, EndedReason, EndedRow,
};

use super::{Freed, answer_retired, ended_reason, free_allocation, reason_of};
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables};
use crate::job_store::ServerJobs;

/// The reason a confirmation records is the job's end: its result's reason when it has one,
/// otherwise its state (a cancel asked for and not yet ended counts as a cancel). It catches
/// a free that records another reason than the job ended for.
#[test]
fn a_confirmation_frees_with_the_reason_of_the_jobs_end() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = jobs
        .end_job(
            &job.id,
            JobEnd::Exited {
                exit_code: 0,
                output: Output::default(),
            },
        )
        .unwrap();
    let with = |state: JobState, reason: Option<ResultReason>| {
        let mut record = ended.clone();
        record.state = state;
        record.result = reason.map(|reason| {
            let mut result = ended.result.clone().unwrap();
            result.reason = reason;
            result
        });
        reason_of(&record)
    };
    let failed = |reason| with(JobState::FailedToStart(reason), None);
    let cases = [
        (with(JobState::Cancelled, None), EndReason::Cancelled),
        (with(JobState::Lost, None), EndReason::HostAbsent),
        (failed(StartFailure::StartTimeout), EndReason::StartTimeout),
        (
            failed(StartFailure::ExclusiveNotReady),
            EndReason::FailedPrepare,
        ),
        (failed(StartFailure::AgentRefused), EndReason::Refused),
        (failed(StartFailure::NoCapacity), EndReason::Refused),
        (failed(StartFailure::InputUnavailable), EndReason::Refused),
        (
            with(JobState::Failed, Some(ResultReason::InputFailed)),
            EndReason::Exited,
        ),
        (
            with(JobState::Succeeded, Some(ResultReason::Exit)),
            EndReason::Exited,
        ),
        (with(JobState::Failed, None), EndReason::Exited),
        (
            with(JobState::Failed, Some(ResultReason::WallLimit)),
            EndReason::WallLimit,
        ),
        (
            with(JobState::Failed, Some(ResultReason::Memory)),
            EndReason::Memory,
        ),
        (
            with(JobState::Failed, Some(ResultReason::Cancelled)),
            EndReason::Cancelled,
        ),
        (with(JobState::Queued, None), EndReason::Cancelled),
        (with(JobState::Placed, None), EndReason::Cancelled),
        (with(JobState::Starting, None), EndReason::Cancelled),
        (with(JobState::Running, None), EndReason::Cancelled),
    ];
    for (index, (found, expected)) in cases.into_iter().enumerate() {
        assert_eq!(found, expected, "case {index}");
    }
}

/// A job whose input failed while it ran ends failed with reason input-failed, and its
/// allocation is freed as exited: its process ran and stopped, the failure being the host's
/// input's, not a stop by the server. One whose input was unavailable at the start is freed as
/// refused, as any start the agent refused. It catches the two new ends of a job's file system
/// freed under another reason (a cancel, a wall limit) than the one they ended for.
#[test]
fn an_input_failure_frees_as_exited_and_an_unavailable_input_as_refused() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = jobs
        .end_job(
            &job.id,
            JobEnd::InputFailed {
                exit_code: Some(1),
                input_failure: "input-unreadable".to_owned(),
                output: Output::default(),
            },
        )
        .unwrap();
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(
        ended.result.as_ref().map(|result| result.reason),
        Some(ResultReason::InputFailed)
    );
    assert_eq!(reason_of(&ended), EndReason::Exited);
    let (refused, _) = placed(&jobs, "job-b", "host-a", spec(), BOOKED);
    let refused = jobs
        .end_job(
            &refused.id,
            JobEnd::Refused {
                failure: StartFailure::InputUnavailable,
                detail: "input-unavailable".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(
        refused.state,
        JobState::FailedToStart(StartFailure::InputUnavailable)
    );
    assert_eq!(reason_of(&refused), EndReason::Refused);
}

/// A confirmation does not free the allocation of a job still meant to run (it is not ended
/// and no cancel was asked for), and writes nothing; a provider's absence frees whatever the
/// job's state; a job whose row was swept frees as exited. It catches a free on the host
/// merely not listing a job that should be running.
#[test]
fn a_confirmation_frees_only_a_job_that_ended_or_was_cancelled() {
    let (store, jobs) = tables();
    let (running, _) = placed(&jobs, "job-running", "host-a", spec(), BOOKED);
    let before = store.commit_stats().txns;
    assert_eq!(
        free_allocation(&jobs, &running.id, Freed::HostConfirmed),
        Err(JobStoreError::NotReleasable {
            id: running.id.clone()
        })
    );
    assert_eq!(store.commit_stats().txns, before);
    let absent = free_allocation(&jobs, &running.id, Freed::ProviderAbsent).unwrap();
    assert_eq!(
        (absent.step, absent.end_reason),
        (AllocationStep::Freed, Some(EndReason::HostAbsent))
    );

    let (swept, _) = placed(&jobs, "job-swept", "host-a", spec(), BOOKED);
    jobs.end_job(&swept.id, JobEnd::Lost).unwrap();
    store
        .txn(Txn::new().delete(keys::job(&swept.id), Cond::Any))
        .unwrap()
        .unwrap();
    let freed = free_allocation(&jobs, &swept.id, Freed::HostConfirmed).unwrap();
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Exited))
    );
}

/// A retired id changes only a job whose live allocation is on the host that retired it: a
/// cancelled job ends cancelled and frees, an ended job frees; an unknown id, a freed
/// allocation, another host's job and a job neither ended nor asked to cancel are left as
/// they are. It catches an answer that revives, frees another host's booking, or ends a job
/// nobody cancelled.
#[test]
fn a_retired_answer_changes_only_a_job_held_on_that_host() {
    let (store, jobs) = tables();
    let host = HostId::new("host-a").unwrap();
    let cancel = |id: &str, on: &str| {
        let (job, _) = placed(&jobs, id, on, spec(), BOOKED);
        jobs.request_cancel(&job.id).unwrap();
        job.id
    };
    let elsewhere = cancel("job-elsewhere", "host-b");
    let freed = cancel("job-freed", "host-a");
    jobs.end_and_free(&freed, JobEnd::Lost, EndReason::HostAbsent)
        .unwrap();
    let (idle, _) = placed(&jobs, "job-idle", "host-a", spec(), BOOKED);
    let before = store.commit_stats().txns;
    for id in [&elsewhere, &freed, &idle.id] {
        answer_retired(&jobs, &host, id).unwrap();
    }
    let unknown = js_jobs::JobId::parse("job-unknown").unwrap();
    answer_retired(&jobs, &host, &unknown).unwrap();
    assert_eq!(store.commit_stats().txns, before);
    let step = |id| jobs.get_allocation(id).unwrap().unwrap().step;
    assert_eq!(step(&elsewhere), AllocationStep::Booked);
    assert_eq!(step(&idle.id), AllocationStep::Booked);

    let cancelled = cancel("job-cancelled", "host-a");
    answer_retired(&jobs, &host, &cancelled).unwrap();
    assert_eq!(
        jobs.get_job(&cancelled).unwrap().unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(step(&cancelled), AllocationStep::Freed);

    let (timed_out, _) = placed(&jobs, "job-timed-out", "host-a", spec(), BOOKED);
    let failure = JobEnd::FailedToStart(StartFailure::StartTimeout);
    jobs.end_job(&timed_out.id, failure).unwrap();
    answer_retired(&jobs, &host, &timed_out.id).unwrap();
    let allocation = jobs.get_allocation(&timed_out.id).unwrap().unwrap();
    assert_eq!(
        (allocation.step, allocation.end_reason),
        (AllocationStep::Freed, Some(EndReason::StartTimeout))
    );
}

/// Job tables over a store whose clock the test moves.
fn timed() -> (Arc<ManualClock>, Arc<Interfere<Fake>>, ServerJobs) {
    let time = Arc::new(ManualClock::default());
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        Arc::clone(&time),
        Flaw::None,
    )));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(Arc::new(dynamic), crate::job_store::ServerIds::random()).unwrap();
    (time, store, jobs)
}

/// Every usage history row `store` holds, oldest end first.
fn history_rows(store: &Interfere<Fake>) -> Vec<EndedRow> {
    let limit = NonZeroUsize::new(100).unwrap();
    EndedHistory::new(store)
        .range(0, u64::MAX, limit)
        .unwrap()
        .rows
}

/// Asserts that freeing a job's allocation, on its host's confirmation or on the provider's
/// absence, writes exactly one usage history row, in the commit that frees: keyed by the
/// free's time and the job id, naming the host, what it booked, its queue wait (submit to
/// placement, at least the 2 s the test waits) and its run time (start to end, at least 5 s),
/// and the reason; that freeing again writes
/// nothing; and that a failed commit leaves neither the free nor the row. It catches a row
/// written in a second transaction, twice, or not at all.
#[test]
fn a_freed_job_allocation_writes_one_usage_row() {
    let cases = [
        (Freed::HostConfirmed, EndedReason::Released),
        (Freed::ProviderAbsent, EndedReason::Lost),
    ];
    for (freed, reason) in cases {
        let (time, store, jobs) = timed();
        let second = Duration::from_secs(1);
        let id = JobId::parse("job-a").unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).unwrap();
        time.advance(2 * second);
        jobs.place(&id, HostId::new("host-a").unwrap(), BOOKED)
            .unwrap();
        time.advance(second);
        jobs.transition(&id, JobState::Placed, JobState::Starting)
            .unwrap();
        time.advance(second);
        jobs.transition(&id, JobState::Starting, JobState::Running)
            .unwrap();
        time.advance(5 * second);
        let exited = JobEnd::Exited {
            exit_code: 0,
            output: Output::default(),
        };
        jobs.end_job(&id, exited).unwrap();
        time.advance(second);
        let usage_keys = || store.scan_prefix("usage/").unwrap().len();

        store.fail_next();
        assert!(free_allocation(&jobs, &id, freed).is_err(), "{freed:?}");
        let step = jobs.get_allocation(&id).unwrap().unwrap().step;
        assert_eq!((step, usage_keys()), (AllocationStep::Releasing, 0));

        let before = store.commit_stats().txns;
        let allocation = free_allocation(&jobs, &id, freed).unwrap();
        assert_eq!(store.commit_stats().txns, before + 1, "{freed:?}");
        // The fake's clock moves a little at each write, so the times are read back from
        // the records: the queue wait is submit to placement, the run start to end.
        let job = jobs.get_job(&id).unwrap().unwrap();
        let ms = |duration: Duration| u64::try_from(duration.as_millis()).unwrap();
        let queued = ms(allocation.created_at.since(job.submitted_at));
        let ran = ms(job.ended_at.unwrap().since(job.started_at.unwrap()));
        let ended = ms(allocation.freed_at.unwrap().as_duration());
        assert!(
            (queued >= 2_000) & (ran >= 5_000),
            "{freed:?}: {queued} {ran}"
        );
        let row = EndedRow {
            id: AllocationId::parse("job-a").unwrap(),
            profile: None,
            host: Some(HostId::new("host-a").unwrap()),
            booked: Some(UsageBooked {
                whole_host: false,
                cpu_milli: 1500,
                memory_bytes: 1 << 30,
            }),
            queued_ms: Some(queued),
            run_ms: Some(ran),
            ended_reason: reason,
            drain_outcome: DrainOutcome::None,
            give_up_outcome: None,
            estimated_end: None,
            actual_end: ended,
            error_ms: None,
            used: None,
        };
        assert_eq!(history_rows(&store), [row], "{freed:?}");
        assert_eq!(usage_keys(), 2, "{freed:?}: the row and its index entry");

        let before = store.commit_stats().txns;
        time.advance(second);
        free_allocation(&jobs, &id, freed).unwrap();
        assert_eq!(
            (store.commit_stats().txns, usage_keys()),
            (before, 2),
            "{freed:?}: a second free wrote"
        );
    }
}

/// Asserts that the history row of a job whose id the history's ids do not take (an
/// upper-case letter) is named by the allocation's id, that a whole-host allocation is
/// recorded as booking the whole host, that a cancel answered by a retired id records a
/// job that never ran (zero run time) for the reason it was freed, that a job whose row was
/// swept is recorded with no queue wait or run time, and the history's reason for every
/// reason an allocation is freed for. It catches a row lost for an id, a wrong booking, and
/// a reason recorded as another.
#[test]
fn the_history_row_of_a_free_names_its_allocation_its_booking_and_its_reason() {
    let (time, store, jobs) = timed();
    let host = HostId::new("host-a").unwrap();
    let (upper, allocation) = placed(&jobs, "Job-Upper", "host-a", spec(), Booked::WholeHost);
    time.advance(Duration::from_secs(1));
    free_allocation(&jobs, &upper.id, Freed::ProviderAbsent).unwrap();
    let hex = allocation.allocation_id.to_string();
    let rows = history_rows(&store);
    assert_eq!(rows[0].id.as_str(), hex);
    let whole = UsageBooked {
        whole_host: true,
        cpu_milli: 0,
        memory_bytes: 0,
    };
    assert_eq!(
        (rows[0].booked, rows[0].run_ms, rows[0].ended_reason),
        (Some(whole), Some(0), EndedReason::Lost)
    );

    let (cancelled, _) = placed(&jobs, "job-cancel", "host-a", spec(), BOOKED);
    jobs.request_cancel(&cancelled.id).unwrap();
    time.advance(Duration::from_secs(1));
    answer_retired(&jobs, &host, &cancelled.id).unwrap();
    let rows = history_rows(&store);
    assert_eq!(rows[1].id.as_str(), "job-cancel");
    assert_eq!(
        (rows[1].run_ms, rows[1].ended_reason),
        (Some(0), EndedReason::Released)
    );

    let (swept, _) = placed(&jobs, "job-swept", "host-a", spec(), BOOKED);
    jobs.end_job(&swept.id, JobEnd::Lost).unwrap();
    store
        .txn(Txn::new().delete(keys::job(&swept.id), Cond::Any))
        .unwrap()
        .unwrap();
    time.advance(Duration::from_secs(1));
    free_allocation(&jobs, &swept.id, Freed::HostConfirmed).unwrap();
    let rows = history_rows(&store);
    assert_eq!(
        (rows[2].id.as_str(), rows[2].queued_ms, rows[2].run_ms),
        ("job-swept", None, None)
    );

    let reasons = [
        (Some(EndReason::Exited), EndedReason::Released),
        (Some(EndReason::Memory), EndedReason::Released),
        (Some(EndReason::Cancelled), EndedReason::Released),
        (None, EndedReason::Released),
        (Some(EndReason::WallLimit), EndedReason::StoppedAtWallLimit),
        (Some(EndReason::HostAbsent), EndedReason::Lost),
        (Some(EndReason::FailedPrepare), EndedReason::FailedToStart),
        (Some(EndReason::StartTimeout), EndedReason::FailedToStart),
        (Some(EndReason::Refused), EndedReason::FailedToStart),
    ];
    for (reason, recorded) in reasons {
        assert_eq!(ended_reason(reason), recorded, "{reason:?}");
    }
}
