//! A job's moves, its cancel, and its allocation's steps from booked to freed.

use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, Txn};

use super::{Rig, host, job_id, rig};
use crate::interfere::INJECTED;
use crate::{
    AllocationStep, EndReason, HostBooking, JobState, JobStore, JobStoreError, RandomIds,
    StartFailure,
};

/// An allocation released and freed after its job ended is kept, freed, with its time and
/// reason, books nothing, and is still there after the backend restarts.
pub fn a_freed_allocation_row_is_kept_as_history<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.running("k");
    rig.tick();
    let ended = rig
        .jobs
        .transition(&id, JobState::Running, JobState::Succeeded)
        .expect("a_freed_allocation_row_is_kept_as_history: the job ends");
    let releasing = rig
        .jobs
        .mark_releasing(&id)
        .expect("a_freed_allocation_row_is_kept_as_history: an ended job's allocation releases");
    assert_eq!(
        releasing.step,
        AllocationStep::Releasing,
        "a_freed_allocation_row_is_kept_as_history: the end did not release the allocation"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking {
            cpu_millis: 2000,
            memory_bytes: 1 << 30,
            disk_bytes: 1 << 32,
            accelerators: 1,
            whole_host: false,
            allocations: 1,
        }),
        "a_freed_allocation_row_is_kept_as_history: a releasing allocation stopped booking"
    );
    rig.tick();
    let freed = rig
        .jobs
        .free(&id, EndReason::Exited)
        .expect("a_freed_allocation_row_is_kept_as_history: the allocation frees");
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Exited)),
        "a_freed_allocation_row_is_kept_as_history: the free is not recorded"
    );
    assert!(
        freed.freed_at > Some(releasing.created_at),
        "a_freed_allocation_row_is_kept_as_history: the free has no time after the booking"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        Some(freed.clone()),
        "a_freed_allocation_row_is_kept_as_history: the freed row is not kept"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking::default()),
        "a_freed_allocation_row_is_kept_as_history: a freed allocation still books"
    );

    let Rig { store, jobs, .. } = rig;
    drop(jobs);
    let store = Arc::into_inner(store)
        .and_then(|wrapped| wrapped.into_inner())
        .expect("nothing else holds the store");
    let jobs = JobStore::new(Arc::new(backend.restart(store)), RandomIds).expect("reopens");
    assert_eq!(
        jobs.get_allocation(&id).expect("reads"),
        Some(freed),
        "a_freed_allocation_row_is_kept_as_history: the history did not survive a restart"
    );
    assert_eq!(
        jobs.get_job(&id).expect("reads"),
        Some(ended),
        "a_freed_allocation_row_is_kept_as_history: the ended job did not survive a restart"
    );
}

/// Freeing a freed allocation answers it as it is, unwritten: the first reason stands. A
/// booked allocation frees directly; a job with no allocation has none to free.
pub fn free_twice_keeps_the_first_reason<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.placed("t");
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Cancelled)
        .expect("ends");
    let first = rig.jobs.free(&id, EndReason::Cancelled).expect("frees");
    let before = rig.raw("alloc/t");
    rig.tick();
    let second = rig
        .jobs
        .free(&id, EndReason::HostAbsent)
        .expect("free_twice_keeps_the_first_reason: a second free answers");
    assert_eq!(
        second, first,
        "free_twice_keeps_the_first_reason: the second free changed the allocation"
    );
    assert_eq!(
        rig.raw("alloc/t"),
        before,
        "free_twice_keeps_the_first_reason: the second free wrote the row"
    );

    let absent = rig.placed("t-absent");
    let freed = rig
        .jobs
        .free(&absent, EndReason::HostAbsent)
        .expect("free_twice_keeps_the_first_reason: a booked allocation frees");
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::HostAbsent)),
        "free_twice_keeps_the_first_reason: a booked allocation did not free"
    );

    let queued = rig.queue("t-queued");
    assert_eq!(
        rig.jobs.free(&queued, EndReason::Exited),
        Err(JobStoreError::NoAllocation { id: queued }),
        "free_twice_keeps_the_first_reason: a job with no allocation freed one"
    );
}

/// An ended job is refused every move, with nothing written; so is a move from a state the
/// job is not in, a move the design does not name, and a move of a job that does not exist.
pub fn an_ended_job_cannot_move_again<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.running("u");
    rig.jobs
        .transition(&id, JobState::Running, JobState::Succeeded)
        .expect("ends");
    let before = rig.snapshot();
    for next in [
        JobState::Running,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Lost,
        JobState::FailedToStart(StartFailure::AgentRefused),
        JobState::Queued,
    ] {
        assert_eq!(
            rig.jobs.transition(&id, JobState::Succeeded, next),
            Err(JobStoreError::IllegalTransition {
                id: id.clone(),
                from: JobState::Succeeded,
                to: next,
            }),
            "an_ended_job_cannot_move_again: an ended job moved to {next:?}"
        );
    }
    assert_eq!(
        rig.jobs
            .transition(&id, JobState::Running, JobState::Failed),
        Err(JobStoreError::UnexpectedState {
            id: id.clone(),
            expected: JobState::Running,
            found: JobState::Succeeded,
        }),
        "an_ended_job_cannot_move_again: a move from a state the job is not in"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "an_ended_job_cannot_move_again: a refused move wrote"
    );

    let placed = rig.placed("u-placed");
    let before = rig.snapshot();
    for next in [JobState::Queued, JobState::Placed, JobState::Running] {
        assert_eq!(
            rig.jobs.transition(&placed, JobState::Placed, next),
            Err(JobStoreError::IllegalTransition {
                id: placed.clone(),
                from: JobState::Placed,
                to: next,
            }),
            "an_ended_job_cannot_move_again: a placed job moved to {next:?}"
        );
    }
    assert_eq!(
        rig.snapshot(),
        before,
        "an_ended_job_cannot_move_again: a refused move wrote"
    );

    let missing = job_id("u-missing");
    let not_found = Err(JobStoreError::NotFound {
        id: missing.clone(),
    });
    assert_eq!(
        rig.jobs
            .transition(&missing, JobState::Queued, JobState::Cancelled)
            .map(drop),
        not_found,
        "an_ended_job_cannot_move_again: a missing job moved"
    );
    assert_eq!(
        rig.jobs.request_cancel(&missing).map(drop),
        not_found,
        "an_ended_job_cannot_move_again: a missing job was cancelled"
    );
    assert_eq!(
        rig.jobs.requeue_failed_prepare(&missing).map(drop),
        not_found,
        "an_ended_job_cannot_move_again: a missing job was requeued"
    );
}

/// Ending a placed job moves its booked allocation to releasing in the same commit. When the
/// commit fails, or the allocation changes before it, the job does not end either. An end
/// with no allocation, or one already releasing, moves the job alone.
pub fn ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit<B: Backend>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "ending_a_placed_job_moves_its_allocation_to_releasing_in_the_same_commit";
    let rig = rig(backend);
    let id = rig.placed("v");
    let commits = rig.store.commit_stats().txns;
    let ended = rig
        .jobs
        .transition(&id, JobState::Placed, JobState::Cancelled)
        .expect("a placed job is cancelled");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the end and the release took more than one commit"
    );
    assert!(ended.ended_at.is_some(), "{name}: the end has no time");
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads"),
        Some(ended),
        "{name}: the end is not what the store holds"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&id)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Releasing),
        "{name}: the allocation was not moved to releasing"
    );

    let failing = rig.placed("v-failing");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .transition(&failing, JobState::Placed, JobState::Lost)
            .map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit is not the store's error"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: a failed end changed the store"
    );

    // A second writer rewrites the allocation after the end read it: the transaction's last
    // precondition fails, after the job's own put.
    let raced = rig.placed("v-raced");
    let inner = rig.store.inner();
    let row = rig.raw("alloc/v-raced").expect("an allocation").value;
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put("alloc/v-raced", row, Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    let answer = rig
        .jobs
        .transition(&raced, JobState::Placed, JobState::Lost);
    assert_eq!(
        rig.jobs
            .get_job(&raced)
            .expect("reads")
            .map(|job| job.state),
        Some(JobState::Placed),
        "{name}: the job ended without its allocation"
    );
    assert_eq!(
        answer,
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: an end over a changed allocation was not stale"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&raced)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Booked),
        "{name}: the stale end moved the allocation"
    );

    let queued = rig.queue("v-queued");
    let gave_up = rig
        .jobs
        .transition(
            &queued,
            JobState::Queued,
            JobState::FailedToStart(StartFailure::NoCapacity),
        )
        .expect("a queued job fails to start");
    assert!(gave_up.ended_at.is_some(), "{name}: the end has no time");
    assert_eq!(
        rig.jobs.get_allocation(&queued).expect("reads"),
        None,
        "{name}: an end with no allocation made one"
    );

    let intent = rig.running("v-intent");
    rig.jobs.request_cancel(&intent).expect("an intent");
    let releasing = rig.jobs.mark_releasing(&intent).expect("releases");
    rig.jobs
        .transition(&intent, JobState::Running, JobState::Cancelled)
        .expect("the host reports it stopped");
    assert_eq!(
        rig.jobs.get_allocation(&intent).expect("reads"),
        Some(releasing),
        "{name}: an allocation already releasing was written again"
    );
}

/// A queued job's cancel ends it at once; a placed, starting or running job's cancel records
/// the intent and nothing else.
pub fn cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent<B: Backend>(
    backend: &B,
) {
    let name = "cancel_of_a_queued_job_is_immediate_and_of_a_placed_job_is_an_intent";
    let rig = rig(backend);
    let queued = rig.queue("x");
    rig.tick();
    let cancelled = rig
        .jobs
        .request_cancel(&queued)
        .expect("a queued job cancels");
    assert_eq!(
        (
            cancelled.state,
            cancelled.cancel_requested,
            cancelled.ended_at.is_some(),
            cancelled.cancel_requested_at.is_some(),
        ),
        (JobState::Cancelled, true, true, true),
        "{name}: a queued job was not cancelled at once"
    );
    assert_eq!(
        rig.jobs.get_job(&queued).expect("reads"),
        Some(cancelled),
        "{name}: the cancel is not what the store holds"
    );
    assert_eq!(
        rig.jobs.get_allocation(&queued).expect("reads"),
        None,
        "{name}: a cancel of a queued job made an allocation"
    );

    let starting = rig.placed("x-starting");
    rig.jobs
        .transition(&starting, JobState::Placed, JobState::Starting)
        .expect("starting");
    for (id, state) in [
        (rig.placed("x-placed"), JobState::Placed),
        (starting, JobState::Starting),
        (rig.running("x-running"), JobState::Running),
    ] {
        let allocation = rig.jobs.get_allocation(&id).expect("reads");
        rig.tick();
        let intent = rig.jobs.request_cancel(&id).expect("a live job cancels");
        assert_eq!(
            (
                intent.state,
                intent.cancel_requested,
                intent.ended_at,
                intent.cancel_requested_at.is_some(),
            ),
            (state, true, None, true),
            "{name}: a cancel of a {state:?} job is not an intent alone"
        );
        assert_eq!(
            rig.jobs.get_job(&id).expect("reads"),
            Some(intent),
            "{name}: the intent is not what the store holds"
        );
        assert_eq!(
            rig.jobs.get_allocation(&id).expect("reads"),
            allocation,
            "{name}: a cancel intent changed the allocation of a {state:?} job"
        );
    }
}

/// A second cancel of a placed job, of a queued (so cancelled) one and of an ended one
/// answers the job as it is and writes nothing.
pub fn cancel_twice_is_cancel_once<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let ended = rig.running("y-ended");
    rig.jobs
        .transition(&ended, JobState::Running, JobState::Failed)
        .expect("ends");
    for (key, id) in [
        ("job/y", rig.placed("y")),
        ("job/y-queued", rig.queue("y-queued")),
        ("job/y-ended", ended),
    ] {
        let first = rig.jobs.request_cancel(&id).expect("a cancel answers");
        let before = rig.raw(key);
        rig.tick();
        let second = rig.jobs.request_cancel(&id).expect("a cancel answers");
        assert_eq!(
            second, first,
            "cancel_twice_is_cancel_once: the second cancel of {id} answered otherwise"
        );
        assert_eq!(
            rig.raw(key),
            before,
            "cancel_twice_is_cancel_once: the second cancel of {id} wrote the row"
        );
    }
    let ended = rig
        .jobs
        .get_job(&job_id("y-ended"))
        .expect("reads")
        .expect("present");
    assert_eq!(
        (ended.state, ended.cancel_requested),
        (JobState::Failed, false),
        "cancel_twice_is_cancel_once: a cancel of an ended job changed it"
    );
}

/// An allocation is released only once its job ended or was asked to cancel; releasing twice
/// is releasing once, and a freed allocation stays freed. A job with no allocation, or none
/// at all, is refused.
pub fn releasing_needs_an_end_or_a_cancel<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.running("z");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.mark_releasing(&id),
        Err(JobStoreError::NotReleasable { id: id.clone() }),
        "releasing_needs_an_end_or_a_cancel: a running job's allocation was released"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "releasing_needs_an_end_or_a_cancel: a refused release wrote"
    );

    rig.jobs.request_cancel(&id).expect("an intent");
    let releasing = rig
        .jobs
        .mark_releasing(&id)
        .expect("releasing_needs_an_end_or_a_cancel: a cancelled job's allocation releases");
    assert_eq!(
        releasing.step,
        AllocationStep::Releasing,
        "releasing_needs_an_end_or_a_cancel: the allocation is not releasing"
    );
    let row = rig.raw("alloc/z");
    assert_eq!(
        rig.jobs.mark_releasing(&id),
        Ok(releasing),
        "releasing_needs_an_end_or_a_cancel: a second release answered otherwise"
    );
    assert_eq!(
        rig.raw("alloc/z"),
        row,
        "releasing_needs_an_end_or_a_cancel: a second release wrote the row"
    );
    let freed = rig.jobs.free(&id, EndReason::Cancelled).expect("frees");
    assert_eq!(
        rig.jobs.mark_releasing(&id),
        Ok(freed),
        "releasing_needs_an_end_or_a_cancel: a freed allocation was released again"
    );

    let queued = rig.queue("z-queued");
    assert_eq!(
        rig.jobs.mark_releasing(&queued),
        Err(JobStoreError::NoAllocation { id: queued }),
        "releasing_needs_an_end_or_a_cancel: a job with no allocation released one"
    );
    let missing = job_id("z-missing");
    assert_eq!(
        rig.jobs.mark_releasing(&missing),
        Err(JobStoreError::NotFound { id: missing }),
        "releasing_needs_an_end_or_a_cancel: a missing job released"
    );
}
