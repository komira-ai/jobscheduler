//! What one host holds ([`crate::JobStore::live_on`]) and the end and free of a job in one
//! commit ([`crate::JobStore::end_and_free`]).

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, Txn};

use super::{host, rig};
use crate::interfere::INJECTED;
use crate::{AllocationStep, EndReason, JobEnd, JobState, JobStoreError, StartFailure, keys};

/// A host's live allocations are its booked and releasing ones, in job id order, each with its
/// job; a freed allocation and another host's are left out, and a releasing allocation whose
/// job's row was deleted at its retention end is listed with no job.
pub fn live_on_lists_the_live_allocations_of_one_host<B: Backend>(backend: &B) {
    let name = "live_on_lists_the_live_allocations_of_one_host";
    let rig = rig(backend);
    let booked = rig.placed("h-booked");
    let releasing = rig.running("h-releasing");
    rig.jobs
        .end_job(&releasing, super::exited(0))
        .expect("the job ends");
    let swept = rig.running("h-swept");
    rig.jobs
        .end_job(&swept, super::exited(1))
        .expect("the job ends");
    rig.delete_raw(&keys::job(&swept));
    let freed = rig.placed("h-freed");
    rig.jobs
        .end_and_free(&freed, super::cancelled(), EndReason::Cancelled)
        .expect("ended and freed");
    let elsewhere = rig.queue("h-elsewhere");
    rig.jobs
        .place(&elsewhere, host("host-b"), super::share())
        .expect("placed on another host");

    let live = rig.jobs.live_on(&host("host-a")).expect("reads");
    let seen: Vec<_> = live
        .iter()
        .map(|(allocation, job)| {
            (
                allocation.job.to_string(),
                allocation.step,
                job.as_ref().map(|job| job.state),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                booked.to_string(),
                AllocationStep::Booked,
                Some(JobState::Placed)
            ),
            (
                releasing.to_string(),
                AllocationStep::Releasing,
                Some(JobState::Succeeded)
            ),
            (swept.to_string(), AllocationStep::Releasing, None),
        ],
        "{name}: the host's live allocations are not listed as they are"
    );
    assert_eq!(
        rig.jobs.live_on(&host("host-c")).expect("reads"),
        Vec::new(),
        "{name}: a host with nothing booked lists something"
    );
}

/// A job's end and its allocation's free are one commit, and a failed or raced commit writes
/// neither; a job already ended keeps its first end and only its allocation is freed; a
/// second call writes nothing; an end the job's state cannot take is refused unwritten.
pub fn end_and_free_is_one_commit<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "end_and_free_is_one_commit";
    let rig = rig(backend);
    let id = rig.placed("e-placed");
    rig.jobs.request_cancel(&id).expect("an intent");
    rig.tick();
    let commits = rig.store.commit_stats().txns;
    let (job, allocation) = rig
        .jobs
        .end_and_free(&id, super::cancelled(), EndReason::Cancelled)
        .expect("ends and frees");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the end and the free took other than one commit"
    );
    assert_eq!(
        (job.state, allocation.step, allocation.end_reason),
        (
            JobState::Cancelled,
            AllocationStep::Freed,
            Some(EndReason::Cancelled)
        ),
        "{name}: the job or its allocation did not end"
    );
    assert!(
        job.ended_at.is_some() && allocation.freed_at == job.ended_at,
        "{name}: the end and the free do not carry the commit's time"
    );
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads"),
        Some(job.clone()),
        "{name}: the end is not what the store holds"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        Some(allocation.clone()),
        "{name}: the free is not what the store holds"
    );
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs
            .end_and_free(&id, JobEnd::Lost, EndReason::HostAbsent),
        Ok((job, allocation)),
        "{name}: a second call did not answer the first end and free"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a second call wrote");

    let ended = rig.running("e-ended");
    let first = rig
        .jobs
        .end_job(&ended, super::exited(3))
        .expect("the job ends");
    let (kept, freed) = rig
        .jobs
        .end_and_free(&ended, super::cancelled(), EndReason::Cancelled)
        .expect("frees");
    assert_eq!(kept, first, "{name}: an ended job's first end was replaced");
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Cancelled)),
        "{name}: an ended job's allocation was not freed"
    );

    let failing = rig.placed("e-failing");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .end_and_free(&failing, super::cancelled(), EndReason::Cancelled)
            .map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit is not the store's error"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed end and free changed the store"
    );

    // A second writer rewrites the allocation after the call read it: neither row is written.
    let raced = rig.placed("e-raced");
    let inner = rig.store.inner();
    let row = rig.raw("alloc/e-raced").expect("an allocation").value;
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put("alloc/e-raced", row, Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    assert_eq!(
        rig.jobs
            .end_and_free(&raced, super::cancelled(), EndReason::Cancelled)
            .map(drop),
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: an end over a changed allocation was not stale"
    );
    assert_eq!(
        rig.jobs
            .get_job(&raced)
            .expect("reads")
            .map(|job| job.state),
        Some(JobState::Placed),
        "{name}: the job ended without its allocation"
    );

    let running = rig.running("e-running");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs
            .end_and_free(
                &running,
                JobEnd::FailedToStart(StartFailure::StartTimeout),
                EndReason::StartTimeout,
            )
            .map(drop),
        Err(JobStoreError::IllegalTransition {
            id: running.clone(),
            from: JobState::Running,
            to: JobState::FailedToStart(StartFailure::StartTimeout),
        }),
        "{name}: an end the job cannot take was not refused"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refused end wrote");

    let queued = rig.queue("e-queued");
    assert_eq!(
        rig.jobs
            .end_and_free(&queued, super::cancelled(), EndReason::Cancelled)
            .map(drop),
        Err(JobStoreError::NoAllocation { id: queued }),
        "{name}: a job with no allocation was ended"
    );
}
