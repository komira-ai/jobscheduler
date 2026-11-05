//! What the placement engine reads each pass ([`crate::JobStore::queued`],
//! [`crate::JobStore::live_allocations`]) and the free that writes its history row in the
//! same commit ([`crate::JobStore::free_with`], [`crate::JobStore::end_and_free_with`]).

use std::cell::Cell;

use js_state::conformance::Backend;
use js_state::{Store, StoreError};

use super::{full_spec, host, job_id, rig};
use crate::codec::FORMAT;
use crate::interfere::INJECTED;
use crate::{AllocationRecord, AllocationStep, EndReason, JobStoreError, NewJob, keys};

fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// The queue is every queued job, the higher priority first, then the earlier submit, then
/// the lower id; a placed or ended job is not in it; a row a later build wrote is left out,
/// a row that does not parse is an error, and so is a failed read.
pub fn queued_lists_the_queue_in_order<B: Backend>(backend: &B) {
    let name = "queued_lists_the_queue_in_order";
    let rig = rig(backend);
    let queue = |text: &str, priority: i32| {
        let new = NewJob {
            id: Some(job_id(text)),
            spec: full_spec(),
            priority,
        };
        rig.jobs.insert_queued(new).expect("recorded");
    };
    queue("q-late-low", 0);
    rig.tick();
    queue("q-high", 5);
    queue("q-b-low", -1);
    queue("q-a-low", -1);
    rig.tick();
    queue("q-later", 0);
    rig.placed("q-placed");
    rig.put_raw("job/q-newer", vec![FORMAT + 1, 0, 1, 0, 0, 0, 0]);
    let order: Vec<String> = rig
        .jobs
        .queued()
        .expect("reads")
        .iter()
        .map(|job| job.id.to_string())
        .collect();
    assert_eq!(
        order,
        ["q-high", "q-late-low", "q-later", "q-a-low", "q-b-low"],
        "{name}: the queue is not in priority, then submit, then id order"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.queued().map(|jobs| jobs.len()),
        Err(injected()),
        "{name}: a failed read is not an error"
    );
    rig.put_raw("job/q-bad", vec![FORMAT, 0]);
    assert!(
        matches!(rig.jobs.queued(), Err(JobStoreError::Corrupt { key, .. }) if key == "job/q-bad"),
        "{name}: a row that does not parse is not an error"
    );
}

/// The live allocations are every booked and releasing allocation of every host, in job id
/// order; a freed one is left out; a row that does not parse is an error, and so is a failed
/// read.
pub fn live_allocations_lists_every_booked_or_releasing_allocation<B: Backend>(backend: &B) {
    let name = "live_allocations_lists_every_booked_or_releasing_allocation";
    let rig = rig(backend);
    rig.placed("l-booked");
    let releasing = rig.running("l-releasing");
    rig.jobs
        .end_job(&releasing, super::exited(0))
        .expect("the job ends");
    let freed = rig.placed("l-freed");
    rig.jobs.free(&freed, EndReason::HostAbsent).expect("freed");
    let elsewhere = rig.queue("l-elsewhere");
    rig.jobs
        .place(&elsewhere, host("host-b"), super::share())
        .expect("placed on another host");
    let seen: Vec<(String, String, AllocationStep)> = rig
        .jobs
        .live_allocations()
        .expect("reads")
        .into_iter()
        .map(|allocation| {
            (
                allocation.job.to_string(),
                allocation.host.to_string(),
                allocation.step,
            )
        })
        .collect();
    let row = |job: &str, host: &str, step| (job.to_owned(), host.to_owned(), step);
    assert_eq!(
        seen,
        [
            row("l-booked", "host-a", AllocationStep::Booked),
            row("l-elsewhere", "host-b", AllocationStep::Booked),
            row("l-releasing", "host-a", AllocationStep::Releasing),
        ],
        "{name}: the live allocations are not listed as they are"
    );
    rig.store.fail_read_after(0);
    assert_eq!(
        rig.jobs.live_allocations().map(|live| live.len()),
        Err(injected()),
        "{name}: a failed read is not an error"
    );
    rig.put_raw(&keys::allocation(&job_id("l-bad")), vec![FORMAT, 0]);
    assert!(
        rig.jobs.live_allocations().is_err(),
        "{name}: an allocation row that does not parse is taken as none"
    );
}

/// The rows given beside a free are written in the commit that frees, for
/// [`crate::JobStore::free_with`] and [`crate::JobStore::end_and_free_with`] alike: a failed
/// commit writes neither the free nor the rows, an allocation already freed writes nothing
/// and is not asked for rows again.
pub fn a_free_with_rows_beside_it_is_one_commit<B: Backend>(backend: &B) {
    let name = "a_free_with_rows_beside_it_is_one_commit";
    let rig = rig(backend);
    let asked = Cell::new(0);
    let beside = |allocation: &AllocationRecord| {
        asked.set(asked.get() + 1);
        let key = format!("usage/ended-id/{}", allocation.job);
        vec![(key, allocation.host.to_string().into_bytes())]
    };
    let id = rig.placed("w-free");
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .free_with(&id, EndReason::Exited, &beside)
            .map(|allocation| allocation.step),
        Err(injected()),
        "{name}: a failed commit is not an error"
    );
    assert_eq!(
        (
            rig.jobs.get_allocation(&id).expect("reads").map(|a| a.step),
            rig.raw("usage/ended-id/w-free")
        ),
        (Some(AllocationStep::Booked), None),
        "{name}: a failed commit wrote the free or its row"
    );
    let commits = rig.store.commit_stats().txns;
    let freed = rig
        .jobs
        .free_with(&id, EndReason::Exited, &beside)
        .expect("frees");
    assert_eq!(
        (rig.store.commit_stats().txns, freed.step),
        (commits + 1, AllocationStep::Freed),
        "{name}: the free and its row took other than one commit"
    );
    assert_eq!(
        rig.raw("usage/ended-id/w-free").map(|row| row.value),
        Some(b"host-a".to_vec()),
        "{name}: the row beside the free was not written"
    );
    let (before, asked_before) = (rig.snapshot(), asked.get());
    assert_eq!(
        rig.jobs.free_with(&id, EndReason::HostAbsent, &beside),
        Ok(freed),
        "{name}: a second free did not answer the first"
    );
    assert_eq!(
        (rig.snapshot(), asked.get()),
        (before, asked_before),
        "{name}: a second free wrote, or asked for rows"
    );

    let cancelled = rig.placed("w-cancel");
    rig.jobs.request_cancel(&cancelled).expect("an intent");
    let commits = rig.store.commit_stats().txns;
    let (_, freed) = rig
        .jobs
        .end_and_free_with(
            &cancelled,
            super::cancelled(),
            EndReason::Cancelled,
            &beside,
        )
        .expect("ends and frees");
    assert_eq!(
        (rig.store.commit_stats().txns, freed.step),
        (commits + 1, AllocationStep::Freed),
        "{name}: the end, the free and the row took other than one commit"
    );
    assert!(
        rig.raw("usage/ended-id/w-cancel").is_some(),
        "{name}: the row beside the end and free was not written"
    );
    let (before, asked_before) = (rig.snapshot(), asked.get());
    rig.jobs
        .end_and_free_with(
            &cancelled,
            super::cancelled(),
            EndReason::Cancelled,
            &beside,
        )
        .expect("answers the first");
    assert_eq!(
        (rig.snapshot(), asked.get()),
        (before, asked_before),
        "{name}: a second end and free wrote, or asked for rows"
    );
}
