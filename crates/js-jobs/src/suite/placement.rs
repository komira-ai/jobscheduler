//! Placement and its allocation, the requeue after a failed whole-host prepare, what a host's
//! allocations book, and that a failed commit writes nothing.

use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, Txn};

use super::{host, job_id, rig, share, whole_host_spec};
use crate::codec::FORMAT;
use crate::interfere::INJECTED;
use crate::{
    AllocationStep, Booked, EndReason, HostBooking, JobState, JobStore, JobStoreError, NewJob,
    RandomIds,
};

/// The error an operation answers when its commit fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// The job becomes placed and its allocation is written in one commit: when the commit
/// fails, or when the allocation row's precondition fails after the job's row in the same
/// transaction, neither row is written.
pub fn placement_and_allocation_commit_together<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let rig = rig(backend);
    let id = rig.queue("p");
    rig.store.fail_next();
    let failed = rig.jobs.place(&id, host("host-a"), share());
    assert_eq!(
        failed,
        Err(injected()),
        "placement_and_allocation_commit_together: a failed commit is not the store's error"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host),
        (JobState::Queued, None),
        "placement_and_allocation_commit_together: a failed placement changed the job"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        None,
        "placement_and_allocation_commit_together: a failed placement left an allocation"
    );

    // A second writer takes the allocation's row after the placement read it absent: the
    // transaction's last precondition fails, after the job's own put.
    let id = rig.queue("q");
    let inner = rig.store.inner();
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put("alloc/q", b"another writer".to_vec(), Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    let raced = rig.jobs.place(&id, host("host-a"), share());
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host),
        (JobState::Queued, None),
        "placement_and_allocation_commit_together: the job was placed without its allocation"
    );
    assert_eq!(
        raced,
        Err(JobStoreError::Stale { id }),
        "placement_and_allocation_commit_together: a placement over a taken allocation"
    );
    assert_eq!(
        rig.raw("alloc/q").map(|row| row.value),
        Some(b"another writer".to_vec()),
        "placement_and_allocation_commit_together: the allocation row was written over"
    );
}

/// Every operation that writes, with its commit failing: it answers the store's error and
/// every row, and the commit counters, read back as before.
pub fn a_failed_commit_changes_nothing<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let queued = rig.queue("f-queued");
    let placed = rig.placed("f-placed");
    let running = rig.running("f-running");
    let ended = rig.placed("f-ended");
    rig.jobs
        .transition(&ended, JobState::Placed, JobState::Cancelled)
        .expect("ends");
    let cancelling = rig.running("f-cancelling");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let whole = rig.queue_spec("f-whole", whole_host_spec());
    rig.jobs
        .place(&whole, host("host-w"), Booked::WholeHost)
        .expect("placed");
    rig.tick();

    type Operation<'a> = Box<dyn Fn() -> Result<(), JobStoreError> + 'a>;
    let jobs = &rig.jobs;
    let operations: [(&str, Operation<'_>); 11] = [
        (
            "an insert with an id",
            Box::new(|| {
                jobs.insert_queued(NewJob {
                    id: Some(job_id("f-new")),
                    spec: super::full_spec(),
                    priority: 0,
                })
                .map(drop)
            }),
        ),
        (
            "an insert with a minted id",
            Box::new(|| {
                jobs.insert_queued(NewJob {
                    id: None,
                    spec: super::full_spec(),
                    priority: 0,
                })
                .map(drop)
            }),
        ),
        (
            "a placement",
            Box::new(|| jobs.place(&queued, host("host-a"), share()).map(drop)),
        ),
        (
            "a move",
            Box::new(|| {
                jobs.transition(&placed, JobState::Placed, JobState::Starting)
                    .map(drop)
            }),
        ),
        (
            "an end",
            Box::new(|| {
                jobs.transition(&running, JobState::Running, JobState::Failed)
                    .map(drop)
            }),
        ),
        (
            "a cancel of a queued job",
            Box::new(|| jobs.request_cancel(&queued).map(drop)),
        ),
        (
            "a cancel of a running job",
            Box::new(|| jobs.request_cancel(&running).map(drop)),
        ),
        (
            "marking releasing",
            Box::new(|| jobs.mark_releasing(&cancelling).map(drop)),
        ),
        (
            "a free",
            Box::new(|| jobs.free(&ended, EndReason::Cancelled).map(drop)),
        ),
        (
            "a requeue",
            Box::new(|| jobs.requeue_failed_prepare(&whole).map(drop)),
        ),
        (
            "an open",
            Box::new(|| JobStore::new(Arc::clone(&rig.store), RandomIds).map(drop)),
        ),
    ];
    for (name, operation) in operations {
        let before = rig.snapshot();
        rig.store.fail_next();
        assert_eq!(
            operation(),
            Err(injected()),
            "a_failed_commit_changes_nothing: {name} with a failed commit answered otherwise"
        );
        assert_eq!(
            rig.snapshot(),
            before,
            "a_failed_commit_changes_nothing: {name} with a failed commit changed the store"
        );
    }
}

/// A second writer cancels the job between the placement's read and its commit: the
/// placement is stale, no allocation is written, and the cancel stands.
pub fn place_on_a_job_that_changed_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let rig = rig(backend);
    let id = rig.queue("s");
    let other = JobStore::new(rig.store.inner(), RandomIds).expect("a second writer");
    let cancelled = id.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&cancelled)
            .expect("the second writer cancels");
    });
    let answer = rig.jobs.place(&id, host("host-a"), share());
    assert_eq!(
        answer,
        Err(JobStoreError::Stale { id: id.clone() }),
        "place_on_a_job_that_changed_is_stale: a placement over a changed job was not stale"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        None,
        "place_on_a_job_that_changed_is_stale: the stale placement left an allocation"
    );
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads").map(|job| job.state),
        Some(JobState::Cancelled),
        "place_on_a_job_that_changed_is_stale: the second writer's cancel was lost"
    );
}

/// Placement refuses a job that is not queued, one whose allocation row still books, and one
/// that does not exist, writing nothing.
pub fn place_refuses_a_job_that_is_not_queued_or_still_booked<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let placed = rig.placed("r-placed");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.place(&placed, host("host-b"), share()),
        Err(JobStoreError::UnexpectedState {
            id: placed.clone(),
            expected: JobState::Queued,
            found: JobState::Placed,
        }),
        "place_refuses_a_job_that_is_not_queued_or_still_booked: a placed job was placed again"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "place_refuses_a_job_that_is_not_queued_or_still_booked: a refused placement wrote"
    );

    // A queued job whose allocation row books (a copy of the placed job's): placing it again
    // would book the host twice.
    let queued = rig.queue("r-queued");
    let booked = rig.raw("alloc/r-placed").expect("an allocation").value;
    rig.put_raw("alloc/r-queued", booked);
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.place(&queued, host("host-b"), share()),
        Err(JobStoreError::AllocationHeld {
            id: queued.clone(),
            step: AllocationStep::Booked,
        }),
        "place_refuses_a_job_that_is_not_queued_or_still_booked: a booked allocation was replaced"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "place_refuses_a_job_that_is_not_queued_or_still_booked: a refused placement wrote"
    );

    let missing = job_id("r-missing");
    assert_eq!(
        rig.jobs.place(&missing, host("host-b"), share()),
        Err(JobStoreError::NotFound { id: missing }),
        "place_refuses_a_job_that_is_not_queued_or_still_booked: a missing job was placed"
    );
}

/// A failed whole-host prepare frees the allocation with `FailedPrepare` and requeues the
/// job; its next placement writes a new booked allocation with a new allocation id. A job that
/// is not whole-host, or whose cancel was asked for, or that is not placed, is not requeued.
pub fn a_requeued_job_gets_a_new_allocation<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let id = rig.queue_spec("w", whole_host_spec());
    let (_, first) = rig
        .jobs
        .place(&id, host("host-a"), Booked::WholeHost)
        .expect("placed");
    rig.tick();
    let (job, freed) = rig
        .jobs
        .requeue_failed_prepare(&id)
        .expect("a_requeued_job_gets_a_new_allocation: a failed prepare requeues");
    assert_eq!(
        (job.state, job.host.clone(), job.placed_at),
        (JobState::Queued, None, None),
        "a_requeued_job_gets_a_new_allocation: the requeued job is not queued and unplaced"
    );
    assert_eq!(
        (freed.step, freed.end_reason, freed.allocation_id),
        (
            AllocationStep::Freed,
            Some(EndReason::FailedPrepare),
            first.allocation_id
        ),
        "a_requeued_job_gets_a_new_allocation: the first allocation was not freed by the prepare"
    );
    assert!(
        freed.freed_at > Some(first.created_at),
        "a_requeued_job_gets_a_new_allocation: the free has no time after the placement"
    );
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads"),
        Some(job),
        "a_requeued_job_gets_a_new_allocation: the requeue is not what the store holds"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking::default()),
        "a_requeued_job_gets_a_new_allocation: the freed allocation still books its host"
    );

    rig.tick();
    let (placed, second) = rig
        .jobs
        .place(&id, host("host-b"), Booked::WholeHost)
        .expect("a_requeued_job_gets_a_new_allocation: a requeued job is placed again");
    assert_ne!(
        second.allocation_id, first.allocation_id,
        "a_requeued_job_gets_a_new_allocation: the new allocation reused the old id"
    );
    assert_eq!(
        (placed.state, second.step, second.host.clone()),
        (JobState::Placed, AllocationStep::Booked, host("host-b")),
        "a_requeued_job_gets_a_new_allocation: the second placement is not booked"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        Some(second),
        "a_requeued_job_gets_a_new_allocation: the new allocation is not the stored one"
    );

    let not_whole = rig.placed("w-share");
    let cancelling = rig.queue_spec("w-cancel", whole_host_spec());
    rig.jobs
        .place(&cancelling, host("host-c"), Booked::WholeHost)
        .expect("placed");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let before = rig.snapshot();
    for refused in [not_whole, cancelling] {
        assert_eq!(
            rig.jobs.requeue_failed_prepare(&refused),
            Err(JobStoreError::IllegalTransition {
                id: refused.clone(),
                from: JobState::Placed,
                to: JobState::Queued,
            }),
            "a_requeued_job_gets_a_new_allocation: {refused} was requeued"
        );
    }
    assert_eq!(
        rig.snapshot(),
        before,
        "a_requeued_job_gets_a_new_allocation: a refused requeue wrote"
    );
    let queued = rig.queue_spec("w-queued", whole_host_spec());
    assert_eq!(
        rig.jobs.requeue_failed_prepare(&queued),
        Err(JobStoreError::UnexpectedState {
            id: queued,
            expected: JobState::Placed,
            found: JobState::Queued,
        }),
        "a_requeued_job_gets_a_new_allocation: a queued job was requeued"
    );
}

/// What a host's live allocations book: shares and the whole host summed, releasing ones
/// counted, freed ones and other hosts' left out; a sum that overflows, and an allocation row
/// this build cannot read, are errors rather than a smaller number.
pub fn booked_on_sums_the_live_allocations_of_a_host<B: Backend>(backend: &B) {
    let rig = rig(backend);
    rig.placed("h-one");
    rig.placed("h-two");
    let whole = rig.queue_spec("h-whole", whole_host_spec());
    rig.jobs
        .place(&whole, host("host-a"), Booked::WholeHost)
        .expect("placed");
    let elsewhere = rig.queue("h-elsewhere");
    rig.jobs
        .place(&elsewhere, host("host-b"), share())
        .expect("placed");
    let freed = rig.placed("h-freed");
    rig.jobs
        .transition(&freed, JobState::Placed, JobState::Cancelled)
        .expect("ends");
    rig.jobs.free(&freed, EndReason::Cancelled).expect("frees");
    let releasing = rig.placed("h-releasing");
    rig.jobs
        .transition(&releasing, JobState::Placed, JobState::Lost)
        .expect("ends");

    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking {
            cpu_millis: 3 * 2000,
            memory_bytes: 3 << 30,
            disk_bytes: 3 << 32,
            accelerators: 3,
            whole_host: true,
            allocations: 4,
        }),
        "booked_on_sums_the_live_allocations_of_a_host: the sum of host-a is wrong"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-z")),
        Ok(HostBooking::default()),
        "booked_on_sums_the_live_allocations_of_a_host: a host with no allocation books"
    );

    let huge = Booked::Shape {
        cpu_millis: u64::MAX,
        memory_bytes: 0,
        disk_bytes: 0,
        accelerators: 0,
    };
    for text in ["h-huge-1", "h-huge-2"] {
        let id = rig.queue(text);
        rig.jobs.place(&id, host("host-d"), huge).expect("placed");
    }
    assert_eq!(
        rig.jobs.booked_on(&host("host-d")),
        Err(JobStoreError::BookingOverflow {
            host: host("host-d")
        }),
        "booked_on_sums_the_live_allocations_of_a_host: an overflowing sum was answered"
    );

    rig.put_raw("alloc/h-newer", vec![FORMAT + 1]);
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Err(JobStoreError::UnknownRow {
            key: "alloc/h-newer".to_owned()
        }),
        "booked_on_sums_the_live_allocations_of_a_host: a row it cannot read was taken as none"
    );
}
