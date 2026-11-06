//! The tick's collection of freed allocations (design section 6.1): a freed allocation's row is
//! deleted once the store's allocation retention has passed since its free, at most one batch
//! of rows a tick and one transaction a batch; a booked or releasing row, a row freed inside
//! the window, a row this build cannot read and the usage history beside a free are kept.

use std::sync::Arc;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::{exited, host, job_id, rig, whole_host_spec};
use crate::codec::{FORMAT, encode_allocation};
use crate::{
    AllocationRecord, AllocationStep, Booked, EndReason, JobId, JobStore, RandomIds, Retention,
    SWEEP_BATCH,
};

/// The allocation retention these checks set: one hour, so the default week is not what is
/// read.
const WINDOW: Duration = Duration::from_secs(3600);

/// The job store of `store` keeping freed allocations for [`WINDOW`].
fn collecting<S: Store>(store: Arc<S>) -> JobStore<S> {
    JobStore::new(store, RandomIds)
        .expect("the job store opens")
        .with_allocation_retention(Retention::new(WINDOW).expect("a window of an hour"))
}

/// The ids of the allocation rows there are, in key order.
fn allocations<S: Store>(store: &S) -> Vec<String> {
    store
        .scan_prefix(crate::keys::ALLOCATION)
        .expect("the store is reachable")
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// Freed allocations past the window go at the next tick and nothing else does: a row whose
/// free is exactly the window ago goes, and one a nanosecond short of it stays until that
/// nanosecond has passed; booked and releasing rows stay however old; a freed row this build
/// cannot read stays; the usage history row written beside a free stays as it was. The window
/// is the store's setting, not the default.
pub fn the_tick_deletes_only_freed_allocations_past_their_window<B: Backend>(backend: &B) {
    let name = "the_tick_deletes_only_freed_allocations_past_their_window";
    let rig = rig(backend);
    let jobs = collecting(Arc::clone(&rig.store));
    let history = |freed: &AllocationRecord| {
        vec![(
            format!("usage/ended/{}", freed.job),
            freed.job.as_str().as_bytes().to_vec(),
        )]
    };
    let free = |text: &str| {
        let id = rig.running(text);
        rig.jobs.end_job(&id, exited(0)).expect("ends");
        let freed = jobs
            .free_with(&id, EndReason::Exited, &history)
            .expect("frees");
        freed.freed_at.expect("a freed allocation has a time")
    };
    let booked = rig.running("gc-booked");
    let releasing = rig.running("gc-releasing");
    rig.jobs.end_job(&releasing, exited(0)).expect("ends");
    free("gc-old");
    let edge = free("gc-edge");
    rig.clock.advance(Duration::from_secs(1));
    let young = free("gc-young");
    // A freed row long past the window, but with a format byte above this build's: the tick
    // cannot know it is freed, so it is kept.
    rig.placed("gc-later");
    let young_row = rig.raw("alloc/gc-young").expect("written").value;
    let mut freed = crate::codec::decode_allocation(&young_row).expect("decodes");
    freed.job = job_id("gc-later");
    let mut newer = encode_allocation(&freed).expect("encodes");
    newer[0] = FORMAT + 1;
    rig.put_raw("alloc/gc-later", newer.clone());
    let usage_before = rig.store.scan_prefix("usage/").expect("reads");
    assert_eq!(
        usage_before.len(),
        3,
        "{name}: the usage rows were not written"
    );
    let advance_to = |at: js_state::StoreTime| rig.clock.advance(at.since(rig.store.now()));

    advance_to(edge.saturating_add(WINDOW));
    jobs.tick().expect("the tick runs");
    let live = vec!["alloc/gc-booked", "alloc/gc-later", "alloc/gc-releasing"];
    assert_eq!(
        allocations(&*rig.store),
        [live.clone(), vec!["alloc/gc-young"]].concat(),
        "{name}: the tick did not delete exactly the freed rows the window ago or earlier"
    );
    advance_to(young.saturating_add(WINDOW - Duration::from_nanos(1)));
    jobs.tick().expect("the tick runs");
    assert_eq!(
        allocations(&*rig.store),
        [live.clone(), vec!["alloc/gc-young"]].concat(),
        "{name}: a row a nanosecond inside the window was deleted"
    );
    rig.clock.advance(Duration::from_nanos(1));
    jobs.tick().expect("the tick runs");
    assert_eq!(
        allocations(&*rig.store),
        live,
        "{name}: a row the window ago stayed"
    );
    assert_eq!(
        rig.raw("alloc/gc-later").map(|row| row.value),
        Some(newer),
        "{name}: a row this build cannot read was written over"
    );
    assert_eq!(
        rig.store.scan_prefix("usage/").expect("reads"),
        usage_before,
        "{name}: the usage history changed"
    );

    rig.clock.advance(Duration::from_secs(365 * 24 * 3600));
    jobs.tick().expect("the tick runs");
    assert_eq!(
        allocations(&*rig.store),
        live,
        "{name}: a live row or an unreadable one went"
    );
    let steps: Vec<AllocationStep> = [&booked, &releasing]
        .into_iter()
        .map(|id| {
            rig.jobs
                .get_allocation(id)
                .expect("reads")
                .expect("kept")
                .step
        })
        .collect();
    assert_eq!(
        steps,
        [AllocationStep::Booked, AllocationStep::Releasing],
        "{name}: a live allocation changed"
    );
}

/// A backlog of more freed rows than a batch is cleared over several ticks, each tick deleting
/// at most [`SWEEP_BATCH`] rows in one commit, and a tick with nothing to collect commits
/// nothing.
pub fn a_backlog_of_freed_allocations_clears_over_several_ticks<B: Backend>(backend: &B) {
    let name = "a_backlog_of_freed_allocations_clears_over_several_ticks";
    let rig = rig(backend);
    let jobs = collecting(Arc::clone(&rig.store));
    let id = rig.running("bl-template");
    rig.jobs.end_job(&id, exited(0)).expect("ends");
    let template = jobs.free(&id, EndReason::Exited).expect("frees");
    let batch = SWEEP_BATCH.get();
    let backlog = batch * 2 + 1;
    let mut bulk = Txn::new();
    for n in 0..backlog {
        let job: JobId = job_id(&format!("bl-{n:04}"));
        let record = AllocationRecord {
            job: job.clone(),
            ..template.clone()
        };
        bulk = bulk.put(
            crate::keys::allocation(&job),
            encode_allocation(&record).expect("encodes"),
            Cond::Any,
        );
    }
    rig.store
        .inner()
        .txn(bulk)
        .expect("reachable")
        .expect("unconditional puts commit");
    rig.reindex();
    rig.clock.advance(WINDOW);

    let mut left = allocations(&*rig.store).len();
    assert_eq!(left, backlog + 1, "{name}: the backlog was not written");
    let mut cleared = Vec::new();
    while left > 0 {
        let commits = rig.store.commit_stats().txns;
        jobs.tick().expect("the tick runs");
        let now_left = allocations(&*rig.store).len();
        assert_eq!(
            rig.store.commit_stats().txns,
            commits + 1,
            "{name}: a tick's collection was not one commit"
        );
        assert!(
            left - now_left <= batch,
            "{name}: a tick deleted {} rows, more than a batch",
            left - now_left
        );
        cleared.push(left - now_left);
        left = now_left;
    }
    assert_eq!(
        cleared,
        [batch, batch, 2],
        "{name}: the backlog did not clear a full batch a tick"
    );
    let commits = rig.store.commit_stats().txns;
    jobs.tick().expect("the tick runs");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "{name}: a tick with nothing to collect committed"
    );
}

/// A collection that loses to a placement of the same job between its read and its commit
/// deletes nothing: the new booking stays, and so does every other row of the batch until the
/// next tick, which deletes the freed ones and not the booking.
pub fn a_collection_that_loses_to_a_placement_keeps_the_booking<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_collection_that_loses_to_a_placement_keeps_the_booking";
    let rig = rig(backend);
    let jobs = collecting(Arc::clone(&rig.store));
    for text in ["race-a", "race-b"] {
        let id = rig.queue_spec(text, whole_host_spec());
        rig.jobs
            .place(&id, host("host-a"), Booked::WholeHost)
            .expect("placed");
        rig.jobs
            .requeue_failed_prepare(&id)
            .expect("a failed prepare requeues the job and frees its allocation");
    }
    rig.clock.advance(WINDOW);
    let other = rig.other();
    let raced = job_id("race-a");
    rig.store.before_next(move || {
        other
            .place(&raced, host("host-b"), Booked::WholeHost)
            .expect("the other writer places the requeued job again");
    });
    jobs.tick().expect("a lost collection is not an error");
    assert_eq!(
        allocations(&*rig.store),
        ["alloc/race-a", "alloc/race-b"],
        "{name}: a collection that lost its race deleted rows"
    );
    jobs.tick().expect("the tick runs");
    let left: Vec<(String, AllocationStep)> = rig
        .store
        .scan_prefix(crate::keys::ALLOCATION)
        .expect("reads")
        .into_iter()
        .map(|(key, row)| {
            let step = crate::codec::decode_allocation(&row.value)
                .expect("decodes")
                .step;
            (key, step)
        })
        .collect();
    assert_eq!(
        left,
        [("alloc/race-a".to_owned(), AllocationStep::Booked)],
        "{name}: the next tick kept a freed row or deleted the booking"
    );
}
