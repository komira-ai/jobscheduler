//! Placement and its allocation, the requeue after a failed whole-host prepare, what a host's
//! allocations book, and that a failed commit writes nothing.

use std::sync::Arc;

use js_state::conformance::Backend;
use js_state::{Cond, ManualClock, Store, StoreError, Txn};

use super::{host, job_id, rig, share, whole_host_spec};
use crate::codec::{FORMAT, alloc_tag, set_field};
use crate::ids::ScriptedIds;
use crate::interfere::{INJECTED, Interfere};
use crate::{
    AllocationId, AllocationStep, Booked, DeclaredPort, EndReason, HostBooking, JobId, JobSpec,
    JobState, JobStore, JobStoreError, MINT_TRIES, NewJob, RandomIds,
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
    rig.jobs.end_job(&ended, super::cancelled()).expect("ends");
    let cancelling = rig.running("f-cancelling");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let whole = rig.queue_spec("f-whole", whole_host_spec());
    rig.jobs
        .place(&whole, host("host-w"), Booked::WholeHost)
        .expect("placed");
    rig.tick();

    type Operation<'a> = Box<dyn Fn() -> Result<(), JobStoreError> + 'a>;
    let jobs = &rig.jobs;
    let operations: [(&str, Operation<'_>); 10] = [
        (
            "an insert with an id",
            Box::new(|| {
                jobs.insert_queued(NewJob {
                    id: Some(job_id("f-new")),
                    spec: super::full_spec(),
                    priority: 0,
                    client: None,
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
                    client: None,
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
            Box::new(|| jobs.end_job(&running, super::exited(1)).map(drop)),
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

    // The first open of an empty store commits the schema row: with that commit failing, it
    // answers the store's error and the store holds no schema row.
    let fresh = Arc::new(Interfere::new(
        backend.open(Arc::new(ManualClock::default())),
    ));
    fresh.fail_next();
    assert_eq!(
        JobStore::new(Arc::clone(&fresh), RandomIds).map(drop),
        Err(injected()),
        "a_failed_commit_changes_nothing: the first open with a failed commit answered otherwise"
    );
    assert_eq!(
        fresh.get(crate::keys::SCHEMA).expect("reads"),
        None,
        "a_failed_commit_changes_nothing: the first open with a failed commit wrote"
    );
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

    // A queued job whose allocation row books or releases (the placed job's row, renamed):
    // placing it again would book the host twice.
    let queued = rig.queue("r-queued");
    let booked = rig.raw("alloc/r-placed").expect("an allocation").value;
    let booked = set_field(&booked, alloc_tag::JOB, b"r-queued").expect("the row parses");
    let releasing = set_field(&booked, alloc_tag::STEP, &[1]).expect("the row parses");
    for (row, step) in [
        (booked, AllocationStep::Booked),
        (releasing, AllocationStep::Releasing),
    ] {
        rig.put_raw("alloc/r-queued", row);
        let before = rig.snapshot();
        assert_eq!(
            rig.jobs.place(&queued, host("host-b"), share()),
            Err(JobStoreError::AllocationHeld {
                id: queued.clone(),
                step,
            }),
            "place_refuses_a_job_that_is_not_queued_or_still_booked: a {step:?} allocation was replaced"
        );
        assert_eq!(
            rig.snapshot(),
            before,
            "place_refuses_a_job_that_is_not_queued_or_still_booked: a refused placement wrote"
        );
    }

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

    // A placed whole-host job whose allocation already ended (the host read absent) or began
    // to release: a requeue would overwrite how it ended.
    let absent = rig.queue_spec("w-absent", whole_host_spec());
    rig.jobs
        .place(&absent, host("host-d"), Booked::WholeHost)
        .expect("placed");
    rig.jobs
        .free(&absent, EndReason::HostAbsent)
        .expect("the host read absent");
    let releasing = rig.queue_spec("w-releasing", whole_host_spec());
    rig.jobs
        .place(&releasing, host("host-e"), Booked::WholeHost)
        .expect("placed");
    let row = rig.raw("alloc/w-releasing").expect("an allocation").value;
    rig.put_raw(
        "alloc/w-releasing",
        set_field(&row, alloc_tag::STEP, &[1]).expect("the row parses"),
    );
    let before = rig.snapshot();
    for (id, step) in [
        (absent.clone(), AllocationStep::Freed),
        (releasing, AllocationStep::Releasing),
    ] {
        assert_eq!(
            rig.jobs.requeue_failed_prepare(&id),
            Err(JobStoreError::AllocationNotBooked {
                id: id.clone(),
                step,
            }),
            "a_requeued_job_gets_a_new_allocation: a job whose allocation is {step:?} was requeued"
        );
    }
    assert_eq!(
        rig.snapshot(),
        before,
        "a_requeued_job_gets_a_new_allocation: a refused requeue wrote"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&absent)
            .expect("reads")
            .and_then(|allocation| allocation.end_reason),
        Some(EndReason::HostAbsent),
        "a_requeued_job_gets_a_new_allocation: the first end of the allocation did not stand"
    );

    let lost_row = rig.queue_spec("w-no-row", whole_host_spec());
    rig.jobs
        .place(&lost_row, host("host-f"), Booked::WholeHost)
        .expect("placed");
    rig.delete_raw("alloc/w-no-row");
    assert_eq!(
        rig.jobs.requeue_failed_prepare(&lost_row),
        Err(JobStoreError::NoAllocation { id: lost_row }),
        "a_requeued_job_gets_a_new_allocation: a job with no allocation row was requeued"
    );
}

/// A source that repeats the freed allocation's id is drawn again, so a placement after a
/// requeue never reuses the id the agent's fence knows the old placement by; a source that
/// only repeats it is given up on, writing nothing.
pub fn a_repeated_allocation_id_is_drawn_again<B: Backend>(backend: &B) {
    let name = "a_repeated_allocation_id_is_drawn_again";
    let rig = rig(backend);
    let first = [0x11; 16];
    let other = [0x22; 16];
    let jobs = rig.minting(ScriptedIds::new(first, [first, other]));
    let id = rig.queue_spec("rep", whole_host_spec());
    let (_, placed) = jobs
        .place(&id, host("host-a"), Booked::WholeHost)
        .expect("placed");
    assert_eq!(placed.allocation_id, AllocationId::from_bits(first));
    jobs.requeue_failed_prepare(&id).expect("requeued");
    let (_, again) = jobs
        .place(&id, host("host-a"), Booked::WholeHost)
        .expect("a_repeated_allocation_id_is_drawn_again: placed again");
    assert_eq!(
        again.allocation_id,
        AllocationId::from_bits(other),
        "{name}: the placement after a requeue reused the earlier allocation id"
    );

    let stuck = rig.minting(ScriptedIds::new(first, []));
    let id = rig.queue_spec("rep-stuck", whole_host_spec());
    stuck
        .place(&id, host("host-a"), Booked::WholeHost)
        .expect("placed");
    stuck.requeue_failed_prepare(&id).expect("requeued");
    let before = rig.snapshot();
    assert_eq!(
        stuck.place(&id, host("host-a"), Booked::WholeHost),
        Err(JobStoreError::IdSpaceExhausted { tries: MINT_TRIES }),
        "{name}: a source of only the earlier id was not given up on"
    );
    assert_eq!(rig.snapshot(), before, "{name}: giving up wrote");
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
    rig.jobs.end_job(&freed, super::cancelled()).expect("ends");
    rig.jobs.free(&freed, EndReason::Cancelled).expect("frees");
    let releasing = rig.placed("h-releasing");
    rig.jobs
        .end_job(&releasing, crate::JobEnd::Lost)
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

/// Placement books the host ports the job declares, sorted and each once, on the allocation
/// it returns and the one the store reads back; a job that declares none books none, and a
/// freed allocation is no longer live. Catches ports that are left off the allocation, kept
/// in declaration order or with a repeat, or still live after the free.
pub fn placement_books_the_jobs_host_ports<B: Backend>(backend: &B) {
    let rig = rig(backend);
    let declared = |host: u16| DeclaredPort {
        host,
        container: 9000,
    };
    let ported = rig.queue_spec(
        "p-a-ported",
        JobSpec {
            ports: vec![declared(8080), declared(80), declared(8080)],
            ..super::full_spec()
        },
    );
    let portless = rig.queue_spec(
        "p-b-portless",
        JobSpec {
            ports: Vec::new(),
            ..super::full_spec()
        },
    );
    let (_, placed) = rig
        .jobs
        .place(&ported, host("host-a"), share())
        .expect("placed");
    rig.jobs
        .place(&portless, host("host-a"), share())
        .expect("placed");
    let name = "placement_books_the_jobs_host_ports";
    assert_eq!(
        placed.host_ports,
        vec![80, 8080],
        "{name}: the returned allocation"
    );
    assert_eq!(
        rig.jobs.get_allocation(&ported).expect("reads"),
        Some(placed.clone()),
        "{name}: the stored allocation"
    );
    let live: Vec<(JobId, Vec<u16>)> = rig
        .jobs
        .live_allocations()
        .expect("reads")
        .into_iter()
        .map(|allocation| (allocation.job, allocation.host_ports))
        .collect();
    assert_eq!(
        live,
        vec![
            (ported.clone(), vec![80, 8080]),
            (portless.clone(), Vec::new())
        ],
        "{name}: the live allocations"
    );
    let on_host: Vec<Vec<u16>> = rig
        .jobs
        .live_on(&host("host-a"))
        .expect("reads")
        .into_iter()
        .map(|(allocation, _)| allocation.host_ports)
        .collect();
    assert_eq!(
        on_host,
        vec![vec![80, 8080], Vec::new()],
        "{name}: live on the host"
    );

    rig.jobs.end_job(&ported, super::cancelled()).expect("ends");
    rig.jobs.free(&ported, EndReason::Cancelled).expect("frees");
    let live: Vec<JobId> = rig
        .jobs
        .live_allocations()
        .expect("reads")
        .into_iter()
        .map(|allocation| allocation.job)
        .collect();
    assert_eq!(live, vec![portless], "{name}: after the free");
}
