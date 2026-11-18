//! The requeue of a running job whose mount provider lost its tree
//! ([`crate::JobStore::requeue_lost_tree`]): the job goes back to queued with its loss counted,
//! its allocation releases, and the next placement waits for the free.

use js_state::Store;
use js_state::conformance::Backend;
use js_types::{JobEventKind, QueueCode, QueueNote};

use super::marks::rewrite;
use super::{host, job_id, rig, share};
use crate::codec::{alloc_tag, set_field};
use crate::{AllocationStep, EndReason, JobId, JobState, JobStoreError, keys};

/// The note a requeue for a lost tree applies.
fn lost_note(on: &str) -> QueueNote {
    QueueNote {
        code: QueueCode::WaitingForCapacity,
        detail: format!("a mount provider on host {on} lost the job's input tree"),
        event: Some(JobEventKind::Requeued),
        host: Some(host(on)),
        attempt: false,
    }
}

/// A running job whose tree was lost goes back to queued in one commit: no host, no placement
/// time, its start time kept for the usage history of the run, one more lost tree, the note applied at the store's time with its event
/// naming the host, its submit time kept, and its allocation releasing. Its failed-prepare
/// count is not touched. The next placement is refused while the allocation releases, and once
/// the host confirms and it is freed, the job is placed again with a new allocation id and the
/// lost-tree count kept.
pub fn a_lost_tree_requeues_a_running_job_and_releases_its_allocation<B: Backend>(backend: &B) {
    let name = "a_lost_tree_requeues_a_running_job_and_releases_its_allocation";
    let rig = rig(backend);
    let id = rig.running("lt-run");
    let first = rig
        .jobs
        .get_allocation(&id)
        .expect("reads")
        .expect("an allocation");
    let before = rig.jobs.get_job(&id).expect("reads").expect("present");
    rig.tick();
    let now = rig.store.now();
    let commits = rig.store.commit_stats().txns;
    let (queued, releasing) = rig
        .jobs
        .requeue_lost_tree(&id, lost_note("host-a"))
        .unwrap_or_else(|why| panic!("{name}: a running job was not requeued: {why}"));
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the requeue took other than one commit"
    );
    assert_eq!(
        (
            queued.state,
            queued.host.clone(),
            queued.placed_at,
            queued.started_at,
            queued.lost_trees,
            queued.failed_prepares,
            queued.submitted_at,
        ),
        (
            JobState::Queued,
            None,
            None,
            before.started_at,
            1,
            0,
            before.submitted_at
        ),
        "{name}: the requeued job is not queued, unplaced, with its start kept and counted once"
    );
    assert_eq!(
        (
            queued.queue.code(),
            queued.queue.since(),
            queued.queue.attempts()
        ),
        (QueueCode::WaitingForCapacity, Some(now), 0),
        "{name}: the note was not applied at the store's time"
    );
    let events = queued.queue.events();
    assert_eq!(
        (events.len(), events[0].kind, events[0].host.clone()),
        (1, JobEventKind::Requeued, Some(host("host-a"))),
        "{name}: the requeue left no event naming the host"
    );
    assert_eq!(
        (releasing.step, releasing.end_reason, releasing.freed_at),
        (AllocationStep::Releasing, None, None),
        "{name}: the allocation is not releasing and unfreed"
    );
    assert_eq!(
        rig.jobs.get_job(&id).expect("reads"),
        Some(queued),
        "{name}: the requeue is not what the store holds"
    );
    assert_eq!(
        rig.jobs.get_allocation(&id).expect("reads"),
        Some(releasing),
        "{name}: the allocation held is not the one returned"
    );

    assert_eq!(
        rig.jobs.place(&id, host("host-a"), share()).map(drop),
        Err(JobStoreError::AllocationHeld {
            id: id.clone(),
            step: AllocationStep::Releasing
        }),
        "{name}: a job was placed over an allocation the host had not freed"
    );
    rig.jobs.free(&id, EndReason::Exited).expect("frees");
    let (placed, second) = rig
        .jobs
        .place(&id, host("host-b"), share())
        .unwrap_or_else(|why| panic!("{name}: a requeued job was not placed again: {why}"));
    assert_ne!(
        second.allocation_id, first.allocation_id,
        "{name}: the new placement reused the lost run's allocation id"
    );
    assert_eq!(
        (placed.state, placed.lost_trees),
        (JobState::Placed, 1),
        "{name}: the placement lost the count"
    );
}

/// A job that is not running (queued, placed, starting), one whose cancel was asked for, a
/// running job whose allocation already releases, or has none, and a job that does not exist
/// are refused with the error that names why, and nothing is written.
pub fn a_lost_tree_requeue_refuses_a_job_that_is_not_running_or_is_cancelling<B: Backend>(
    backend: &B,
) {
    let name = "a_lost_tree_requeue_refuses_a_job_that_is_not_running_or_is_cancelling";
    let rig = rig(backend);
    let queued = rig.queue("lr-queued");
    let placed = rig.placed("lr-placed");
    let cancelling = rig.running("lr-cancel");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let releasing = rig.running("lr-releasing");
    let key = keys::allocation(&releasing);
    let row = rig.raw(&key).expect("an allocation").value;
    rig.put_raw(
        &key,
        set_field(&row, alloc_tag::STEP, &[1]).expect("the row parses"),
    );
    let no_row = rig.running("lr-no-row");
    rig.delete_raw(&keys::allocation(&no_row));
    let missing = job_id("lr-missing");

    let not_running = |id: &JobId, found| JobStoreError::UnexpectedState {
        id: id.clone(),
        expected: JobState::Running,
        found,
    };
    let cases = [
        (queued.clone(), not_running(&queued, JobState::Queued)),
        (placed.clone(), not_running(&placed, JobState::Placed)),
        (
            cancelling.clone(),
            JobStoreError::IllegalTransition {
                id: cancelling,
                from: JobState::Running,
                to: JobState::Queued,
            },
        ),
        (
            releasing.clone(),
            JobStoreError::AllocationNotBooked {
                id: releasing,
                step: AllocationStep::Releasing,
            },
        ),
        (no_row.clone(), JobStoreError::NoAllocation { id: no_row }),
        (missing.clone(), JobStoreError::NotFound { id: missing }),
    ];
    let before = rig.snapshot();
    for (id, refusal) in cases {
        assert_eq!(
            rig.jobs
                .requeue_lost_tree(&id, lost_note("host-a"))
                .map(drop),
            Err(refusal),
            "{name}: {id} was not refused as it should be"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a refused requeue wrote");
}

/// A requeue whose commit fails writes nothing, and one whose job row or allocation row
/// another writer changed first is stale and writes neither row.
pub fn a_lost_tree_requeue_loses_to_a_change_of_either_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_lost_tree_requeue_loses_to_a_change_of_either_row";
    let rig = rig(backend);
    let id = rig.running("ls-run");
    let before = rig.snapshot().0;
    rig.store.fail_next();
    assert!(
        rig.jobs
            .requeue_lost_tree(&id, lost_note("host-a"))
            .is_err(),
        "{name}: a failed commit was not an error"
    );
    assert_eq!(rig.snapshot().0, before, "{name}: a failed commit wrote");

    for key in [keys::job(&id), keys::allocation(&id)] {
        rig.store.before_next(rewrite(&rig, &key));
        assert_eq!(
            rig.jobs
                .requeue_lost_tree(&id, lost_note("host-a"))
                .map(drop),
            Err(JobStoreError::Stale { id: id.clone() }),
            "{name}: a requeue over a changed {key} was not stale"
        );
        let job = rig.jobs.get_job(&id).expect("reads").expect("present");
        let allocation = rig.jobs.get_allocation(&id).expect("reads");
        assert_eq!(
            (job.state, allocation.map(|row| row.step)),
            (JobState::Running, Some(AllocationStep::Booked)),
            "{name}: a stale requeue wrote a row ({key})"
        );
    }
}
