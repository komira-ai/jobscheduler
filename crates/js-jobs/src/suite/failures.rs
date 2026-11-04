//! A failed read, and the schema row raced or unreadable at open: each answers the store's
//! error and writes nothing.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Cond, ManualClock, Store, StoreError, Txn};

use super::{full_spec, host, rig, share, whole_host_spec};
use crate::interfere::{INJECTED, Interfere};
use crate::keys;
use crate::{Booked, EndReason, JobState, JobStore, JobStoreError, NewJob, RandomIds};

/// The error an operation answers when its read fails.
fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// Every operation, with each of its reads failing in turn: it answers the store's error,
/// and every row and the commit counters read back as before, so nothing was written after
/// a failed read (the second read of a two-row operation included).
pub fn a_failed_read_changes_nothing<B: Backend>(backend: &B) {
    let name = "a_failed_read_changes_nothing";
    let rig = rig(backend);
    let queued = rig.queue("fr-queued");
    let placed = rig.placed("fr-placed");
    let running = rig.running("fr-running");
    let cancelling = rig.running("fr-cancelling");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let whole = rig.queue_spec("fr-whole", whole_host_spec());
    rig.jobs
        .place(&whole, host("host-w"), Booked::WholeHost)
        .expect("placed");
    rig.tick();

    type Operation<'a> = Box<dyn Fn() -> Result<(), JobStoreError> + 'a>;
    let jobs = &rig.jobs;
    let one = NonZeroUsize::MIN;
    let operations: [(&str, usize, Operation<'_>); 14] = [
        (
            "an insert",
            1,
            Box::new(|| {
                jobs.insert_queued(NewJob {
                    id: Some(super::job_id("fr-new")),
                    spec: full_spec(),
                    priority: 0,
                })
                .map(drop)
            }),
        ),
        (
            "a placement",
            2,
            Box::new(|| jobs.place(&queued, host("host-a"), share()).map(drop)),
        ),
        (
            "a move",
            1,
            Box::new(|| {
                jobs.transition(&placed, JobState::Placed, JobState::Starting)
                    .map(drop)
            }),
        ),
        (
            "an end",
            2,
            Box::new(|| jobs.end_job(&running, super::exited(1)).map(drop)),
        ),
        (
            "a requeue",
            2,
            Box::new(|| jobs.requeue_failed_prepare(&whole).map(drop)),
        ),
        (
            "a cancel",
            2,
            Box::new(|| jobs.request_cancel(&queued).map(drop)),
        ),
        (
            "a release",
            2,
            Box::new(|| jobs.mark_releasing(&cancelling).map(drop)),
        ),
        (
            "a free",
            1,
            Box::new(|| jobs.free(&running, EndReason::Exited).map(drop)),
        ),
        (
            "a job read",
            1,
            Box::new(|| jobs.get_job(&queued).map(drop)),
        ),
        (
            "an allocation read",
            1,
            Box::new(|| jobs.get_allocation(&placed).map(drop)),
        ),
        (
            "a list",
            1,
            Box::new(|| jobs.list_jobs(None, None, one).map(drop)),
        ),
        (
            "a filtered list",
            1,
            Box::new(|| jobs.list_jobs(Some(JobState::Queued), None, one).map(drop)),
        ),
        (
            "a sum of bookings",
            1,
            Box::new(|| jobs.booked_on(&host("host-a")).map(drop)),
        ),
        (
            "an open",
            1,
            Box::new(|| JobStore::new(Arc::clone(&rig.store), RandomIds).map(drop)),
        ),
    ];
    for (what, reads, operation) in operations {
        for read in 0..reads {
            let before = rig.snapshot();
            rig.store.fail_read_after(read);
            assert_eq!(
                operation(),
                Err(injected()),
                "{name}: {what} with read {read} failing answered otherwise"
            );
            assert_eq!(
                rig.snapshot(),
                before,
                "{name}: {what} with read {read} failing changed the store"
            );
        }
    }
}

/// A store that writes the schema row between this one's read and its commit is the one whose
/// version is read; a failed read after that lost commit is the store's error, and the only
/// schema row is the other store's.
pub fn opening_after_another_store_wrote_the_schema_reads_its_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "opening_after_another_store_wrote_the_schema_reads_its_row";
    for fail_the_readback in [false, true] {
        let store = Arc::new(Interfere::new(
            backend.open(Arc::new(ManualClock::default())),
        ));
        let inner = store.inner();
        store.before_next(move || {
            inner
                .txn(Txn::new().put(keys::SCHEMA, 7u32.to_be_bytes().to_vec(), Cond::Any))
                .expect("reachable")
                .expect("commits");
        });
        // The open's first read passes; its second, after the lost commit, fails or not.
        store.fail_read_after(if fail_the_readback { 1 } else { usize::MAX });
        let opened = JobStore::new(Arc::clone(&store), RandomIds).map(|jobs| jobs.stored_schema());
        let want = if fail_the_readback {
            Err(injected())
        } else {
            Ok(7)
        };
        assert_eq!(
            opened, want,
            "{name}: the open did not read the other store's schema row"
        );
        assert_eq!(
            store.get(keys::SCHEMA).expect("reads").map(|row| row.value),
            Some(7u32.to_be_bytes().to_vec()),
            "{name}: the open wrote over the other store's schema row"
        );
    }
}
