//! A second writer between an operation's read and its commit: every operation pins the
//! versions it read, answers `Stale` when one moved, and leaves the winner's rows as they are.
//! The race is a hook that commits just before the operation's own commit.

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::{host, rig, share, whole_host_spec};
use crate::{AllocationStep, Booked, EndReason, JobState, JobStoreError};

/// A hook that rewrites the row `key` with the bytes it holds now: the same value at a new
/// version, as any other writer's commit leaves it.
fn rewrite<S: Store + 'static>(
    store: std::sync::Arc<S>,
    key: String,
    value: Vec<u8>,
) -> impl FnOnce() + Send + 'static {
    move || {
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("reachable")
            .expect("commits");
    }
}

/// A requeue whose job, or whose allocation, another writer changed is stale: neither row is
/// written. A store that applies the job's put and then fails the allocation's precondition
/// leaves the job requeued with its allocation still booked, which this check rejects.
pub fn requeue_loses_to_a_change_of_either_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "requeue_loses_to_a_change_of_either_row";
    let rig = rig(backend);
    let raced = rig.queue_spec("rq-allocation", whole_host_spec());
    rig.jobs
        .place(&raced, host("host-a"), Booked::WholeHost)
        .expect("placed");
    let key = "alloc/rq-allocation".to_owned();
    let row = rig.raw(&key).expect("written").value;
    rig.store
        .before_next(rewrite(rig.store.inner(), key.clone(), row.clone()));
    let answer = rig.jobs.requeue_failed_prepare(&raced);
    assert_eq!(
        rig.jobs
            .get_job(&raced)
            .expect("reads")
            .map(|job| job.state),
        Some(JobState::Placed),
        "{name}: the job was requeued without its allocation"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: a requeue over a changed allocation was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        Some(row),
        "{name}: the other writer's allocation row was written over"
    );

    let cancelled = rig.queue_spec("rq-job", whole_host_spec());
    rig.jobs
        .place(&cancelled, host("host-b"), Booked::WholeHost)
        .expect("placed");
    let other = rig.other();
    let target = cancelled.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs.requeue_failed_prepare(&cancelled).map(drop),
        Err(JobStoreError::Stale {
            id: cancelled.clone()
        }),
        "{name}: a requeue over a changed job was not stale"
    );
    let job = rig
        .jobs
        .get_job(&cancelled)
        .expect("reads")
        .expect("present");
    assert_eq!(
        (job.state, job.cancel_requested),
        (JobState::Placed, true),
        "{name}: the other writer's cancel was lost"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&cancelled)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Booked),
        "{name}: the stale requeue freed the allocation"
    );
}

/// A move whose job another writer changed (a cancel intent) is stale, and the intent stands.
pub fn a_move_that_loses_its_job_version_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_move_that_loses_its_job_version_is_stale";
    let rig = rig(backend);
    let id = rig.placed("mv");
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs
            .transition(&id, JobState::Placed, JobState::Starting)
            .map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a move over a changed job was not stale"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.cancel_requested),
        (JobState::Placed, true),
        "{name}: the move or the lost cancel was written"
    );
}

/// A cancel of a queued job that another writer placed meanwhile is stale: the placement and
/// its allocation stand, and the cancel tried again is an intent on the placed job.
pub fn a_cancel_that_loses_to_a_placement_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_cancel_that_loses_to_a_placement_is_stale";
    let rig = rig(backend);
    let id = rig.queue("cp");
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .place(&target, host("host-b"), share())
            .expect("the other writer places");
    });
    assert_eq!(
        rig.jobs.request_cancel(&id).map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a cancel over a placement was not stale"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host, job.cancel_requested),
        (JobState::Placed, Some(host("host-b")), false),
        "{name}: the placement was lost or the cancel written"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&id)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Booked),
        "{name}: the placement's allocation was lost"
    );
    let again = rig.jobs.request_cancel(&id).expect("the cancel again");
    assert_eq!(
        (again.state, again.cancel_requested),
        (JobState::Placed, true),
        "{name}: the cancel tried again is not an intent"
    );
}

/// A release that another writer's free beats is stale, and the allocation stays freed with
/// the free's reason; tried again it answers the freed allocation.
pub fn a_release_that_loses_to_a_free_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_release_that_loses_to_a_free_is_stale";
    let rig = rig(backend);
    let id = rig.running("rf");
    rig.jobs.request_cancel(&id).expect("an intent");
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .free(&target, EndReason::HostAbsent)
            .expect("the other writer frees");
    });
    assert_eq!(
        rig.jobs.mark_releasing(&id).map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a release over a free was not stale"
    );
    let allocation = rig
        .jobs
        .get_allocation(&id)
        .expect("reads")
        .expect("present");
    assert_eq!(
        (allocation.step, allocation.end_reason),
        (AllocationStep::Freed, Some(EndReason::HostAbsent)),
        "{name}: the free was lost"
    );
    assert_eq!(
        rig.jobs.mark_releasing(&id),
        Ok(allocation),
        "{name}: the release tried again did not answer the freed allocation"
    );
}

/// Two frees with different reasons: the one that commits first stands, the other is stale,
/// and tried again it answers the first one's reason.
pub fn concurrent_frees_keep_the_first_committed_reason<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "concurrent_frees_keep_the_first_committed_reason";
    let rig = rig(backend);
    let id = rig.placed("ff");
    rig.jobs.end_job(&id, super::cancelled()).expect("ends");
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .free(&target, EndReason::HostAbsent)
            .expect("the other writer frees");
    });
    assert_eq!(
        rig.jobs.free(&id, EndReason::Cancelled).map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: the second free was not stale"
    );
    let retried = rig
        .jobs
        .free(&id, EndReason::Cancelled)
        .expect("tried again");
    assert_eq!(
        retried.end_reason,
        Some(EndReason::HostAbsent),
        "{name}: the first committed reason did not stand"
    );
}

/// A placement whose freed allocation another writer changed after the placement read it is
/// stale: the job stays queued and the other row stands.
pub fn a_placement_over_a_changed_freed_allocation_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_placement_over_a_changed_freed_allocation_is_stale";
    let rig = rig(backend);
    let id = rig.queue_spec("pf", whole_host_spec());
    rig.jobs
        .place(&id, host("host-a"), Booked::WholeHost)
        .expect("placed");
    rig.jobs.requeue_failed_prepare(&id).expect("requeued");
    let key = "alloc/pf".to_owned();
    let row = rig.raw(&key).expect("written").value;
    let before = rig.raw(&key).expect("written").version;
    rig.store
        .before_next(rewrite(rig.store.inner(), key.clone(), row.clone()));
    assert_eq!(
        rig.jobs
            .place(&id, host("host-b"), Booked::WholeHost)
            .map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a placement over a changed allocation was not stale"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("present");
    assert_eq!(
        (job.state, job.host),
        (JobState::Queued, None),
        "{name}: the stale placement changed the job"
    );
    let now = rig.raw(&key).expect("written");
    assert_eq!(
        (now.value, now.version != before),
        (row, true),
        "{name}: the other writer's row was written over"
    );
}
