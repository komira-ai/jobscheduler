//! The one way an allocation is freed, and the answer to a retired id.

use js_jobs::{
    AllocationStep, EndReason, JobEnd, JobState, JobStoreError, Output, ResultReason, StartFailure,
    keys,
};
use js_state::{Cond, Store, Txn};
use js_types::HostId;

use super::{Freed, answer_retired, free_allocation, reason_of};
use crate::agent_api::job_rig::{BOOKED, placed, spec, tables};

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
