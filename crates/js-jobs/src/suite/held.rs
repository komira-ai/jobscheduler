//! The requeue after a start a host refused ([`crate::JobStore::requeue_held`]): one
//! commit that puts a placed or starting job, of a share of a host or of the whole host, back
//! to queued with the bars and note it is given and the host appended to its held refusals or
//! its capacity refusals (or to neither, for a bundle-pending refusal),
//! frees the allocation with `Refused`, writes the rows given beside it and deletes the
//! allocation's start from the host's dispatch queue without queueing a stop. It counts no
//! failed prepare, writes nothing when its commit fails, is refused for a job or allocation it
//! cannot requeue, and is stale when a row it read changed.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::Store;
use js_state::conformance::Backend;
use js_types::{JobEventKind, QueueCode, QueueNote};

use super::marks::{rewrite, state_of};
use super::whole::{history, injected, later, placed_whole, queue_of};
use super::{Rig, host, job_id, rig, share};
use crate::codec::{alloc_tag, set_field};
use crate::keys;
use crate::{
    AllocationRecord, AllocationStep, Beside, EndReason, Exclusion, HeldRequeue, HostBooking,
    JobId, JobRecord, JobState, JobStoreError, RequeuedFor,
};

/// The requeue the server makes of a job `host-a` refused: the host barred for an hour beside
/// a bar for life the job had on `host-x`, and a note naming the host.
fn held_on_a<S: Store>(rig: &Rig<S>) -> HeldRequeue {
    HeldRequeue {
        refused: RequeuedFor::Held,
        prepare_bars: BTreeMap::from([
            (host("host-a"), Exclusion::Until(later(rig, 3600))),
            (host("host-x"), Exclusion::ForLife),
        ]),
        queue: Some(QueueNote {
            code: QueueCode::WaitingForCapacity,
            detail: "host host-a is held".to_owned(),
            event: Some(JobEventKind::Requeued),
            host: Some(host("host-a")),
            attempt: false,
        }),
    }
}

/// The messages of `host`'s dispatch queue, by kind and job.
fn dispatch_of<S: Store>(rig: &Rig<S>, on: &str) -> Vec<(crate::DispatchKind, JobId)> {
    let queue = rig.jobs.dispatch_queue(&host(on)).expect("reads the queue");
    queue
        .into_iter()
        .map(|message| (message.kind, message.job))
        .collect()
}

/// A placed job of a share of `host-a` whose start the host refused as held goes back to
/// queued in one commit: no host and no placement time, its submit time and so its place in
/// the queue kept, the bars and the note given, the host appended to its held refusals and its
/// failed prepares not counted; its allocation is freed with `Refused` at the commit's time,
/// with the rows given beside it, and its start leaves the host's dispatch queue with no stop
/// queued. Its next placement gets a new allocation, and a second refusal, of a starting job
/// whose start was handed out, appends the second host after the first. A whole-host job
/// refused while starting goes back the same way.
pub fn a_held_refusal_requeues_the_job_without_counting_a_failure<B: Backend>(backend: &B) {
    let name = "a_held_refusal_requeues_the_job_without_counting_a_failure";
    let rig = rig(backend);
    let id = rig.placed("hr-first");
    rig.tick();
    let second = rig.queue("hr-second");
    let placed = rig.jobs.get_job(&id).expect("reads").expect("recorded");
    let first = rig
        .jobs
        .get_allocation(&id)
        .expect("reads")
        .expect("booked");
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [(crate::DispatchKind::Start, id.clone())],
        "{name}: the placement queued no start"
    );
    rig.tick();
    let now = rig.store.now().expect("the store's clock");
    let requeue = held_on_a(&rig);
    let commits = rig.store.commit_stats().txns;
    let (queued, freed) = rig
        .jobs
        .requeue_held(&id, requeue.clone(), &history)
        .expect("a refused start requeues");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the requeue took other than one commit"
    );
    let mut trail = placed.queue.clone();
    trail.apply_all(requeue.queue.as_slice(), now);
    assert_eq!(
        queued,
        JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            prepare_bars: requeue.prepare_bars.clone(),
            held_refusals: vec![host("host-a")],
            queue: trail,
            ..placed
        },
        "{name}: the job is not queued with the bars, the note and the host, and nothing else"
    );
    assert_eq!(
        (
            queued.failed_prepares,
            queued.queue.events().last().map(|e| e.kind)
        ),
        (0, Some(JobEventKind::Requeued)),
        "{name}: a failed prepare was counted or the note's event not appended"
    );
    assert_eq!(
        freed,
        AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::Refused),
            ..first.clone()
        },
        "{name}: the allocation is not freed with the refusal's reason"
    );
    assert_eq!(rig.jobs.get_job(&id), Ok(Some(queued)));
    assert_eq!(rig.jobs.get_allocation(&id), Ok(Some(freed)));
    assert_eq!(
        rig.raw(&format!("usage/ended-id/{id}"))
            .map(|row| row.value),
        Some(b"ended".to_vec()),
        "{name}: the row beside the free was not written"
    );
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [],
        "{name}: the start was left in the dispatch queue or a stop was queued"
    );
    assert_eq!(
        queue_of(&rig),
        [id.clone(), second],
        "{name}: the requeued job lost its place in the queue"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking::default()),
        "{name}: the freed allocation still books its host"
    );

    rig.tick();
    let (_, next) = rig
        .jobs
        .place(&id, host("host-b"), share())
        .expect("placed again");
    assert_ne!(
        next.allocation_id, first.allocation_id,
        "{name}: the placement after the requeue kept the freed allocation's id"
    );
    rig.jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("placed to starting");
    let claimed = rig
        .jobs
        .claim_dispatch(&host("host-b"), "server-1", Duration::from_secs(30))
        .expect("hands the start out");
    assert_eq!(claimed.len(), 1, "{name}: the second start was not queued");
    let bars = BTreeMap::from([(host("host-b"), Exclusion::Until(later(&rig, 3600)))]);
    let again = HeldRequeue {
        refused: RequeuedFor::Held,
        prepare_bars: bars.clone(),
        queue: None,
    };
    let (twice, _) = rig
        .jobs
        .requeue_held(&id, again, &|_| Vec::new())
        .expect("a starting job's refused start requeues");
    assert_eq!(
        (twice.state, twice.held_refusals.clone(), twice.prepare_bars),
        (JobState::Queued, vec![host("host-a"), host("host-b")], bars),
        "{name}: the second refusal did not follow the first"
    );
    assert_eq!(
        dispatch_of(&rig, "host-b"),
        [],
        "{name}: a handed-out start was left or a stop queued for a host that refused it"
    );

    let (whole, _, _) = placed_whole(&rig, "hr-whole");
    rig.jobs
        .transition(&whole, JobState::Placed, JobState::Starting)
        .expect("the host is ready");
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [(crate::DispatchKind::Start, whole.clone())],
        "{name}: the whole-host job's start was not queued"
    );
    let (requeued, freed) = rig
        .jobs
        .requeue_held(&whole, held_on_a(&rig), &|_| Vec::new())
        .expect("a starting whole-host job requeues");
    assert_eq!(
        (requeued.state, requeued.held_refusals, freed.step),
        (
            JobState::Queued,
            vec![host("host-a")],
            AllocationStep::Freed
        ),
        "{name}: the whole-host job did not go back to queued"
    );
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [],
        "{name}: the whole start was left"
    );
    assert_eq!(
        rig.jobs.booked_on(&host("host-a")),
        Ok(HostBooking::default()),
        "{name}: the whole host is still booked"
    );
}

/// The requeue is refused, writing nothing and asking for no rows, for a job that is queued,
/// running or ended, for one whose cancel was asked for (it is cancelled, not requeued), for an
/// allocation that is freed, releasing or missing, and for a job that does not exist; a requeue
/// that lands asks for its rows once.
pub fn a_held_refusal_refuses_what_it_cannot_requeue<B: Backend>(backend: &B) {
    let name = "a_held_refusal_refuses_what_it_cannot_requeue";
    let rig = rig(backend);
    let queued = rig.queue("hn-queued");
    let running = rig.running("hn-running");
    let ended = rig.placed("hn-ended");
    rig.jobs
        .end_job(&ended, crate::JobEnd::Lost)
        .expect("the job is lost");
    let cancelling = rig.placed("hn-cancel");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let freed = rig.placed("hn-freed");
    rig.jobs
        .free(&freed, EndReason::HostAbsent)
        .expect("the host read absent");
    let releasing = rig.placed("hn-releasing");
    let key = keys::allocation(&releasing);
    let row = rig.raw(&key).expect("an allocation").value;
    rig.put_raw(
        &key,
        set_field(&row, alloc_tag::STEP, &[1]).expect("the row parses"),
    );
    let no_row = rig.placed("hn-no-row");
    rig.delete_raw(&keys::allocation(&no_row));
    let missing = job_id("hn-missing");

    let state = |id: &JobId, found| JobStoreError::UnexpectedState {
        id: id.clone(),
        expected: JobState::Placed,
        found,
    };
    let not_booked = |id: &JobId, step| JobStoreError::AllocationNotBooked {
        id: id.clone(),
        step,
    };
    let cases = [
        (queued.clone(), state(&queued, JobState::Queued)),
        (running.clone(), state(&running, JobState::Running)),
        (ended.clone(), state(&ended, JobState::Lost)),
        (
            cancelling.clone(),
            JobStoreError::IllegalTransition {
                id: cancelling,
                from: JobState::Placed,
                to: JobState::Queued,
            },
        ),
        (freed.clone(), not_booked(&freed, AllocationStep::Freed)),
        (
            releasing.clone(),
            not_booked(&releasing, AllocationStep::Releasing),
        ),
        (no_row.clone(), JobStoreError::NoAllocation { id: no_row }),
        (missing.clone(), JobStoreError::NotFound { id: missing }),
    ];
    let before = rig.snapshot();
    let asked = std::cell::Cell::new(0);
    let beside: &Beside<'_> = &|_| {
        asked.set(asked.get() + 1);
        Vec::new()
    };
    for (id, refusal) in cases {
        assert_eq!(
            rig.jobs
                .requeue_held(&id, held_on_a(&rig), beside)
                .map(drop),
            Err(refusal),
            "{name}: {id} was not refused as it should be"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a refused requeue wrote");
    assert_eq!(asked.get(), 0, "{name}: a refused requeue asked for rows");

    let placed = rig.placed("hn-placed");
    rig.jobs
        .requeue_held(&placed, held_on_a(&rig), beside)
        .expect("a placed job's refused start requeues");
    assert_eq!(
        asked.get(),
        1,
        "{name}: a requeue that lands did not ask for its rows once"
    );
}

/// A requeue whose commit fails writes nothing; one whose job or allocation another writer
/// changed is stale and writes neither row nor the rows beside. A store that applies the job's
/// put and then fails the allocation's precondition leaves the job queued with its allocation
/// still booked, which this check rejects.
pub fn a_held_refusal_loses_to_a_change_of_either_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_held_refusal_loses_to_a_change_of_either_row";
    let rig = rig(backend);
    let failing = rig.placed("hl-failing");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .requeue_held(&failing, held_on_a(&rig), &history)
            .map(drop),
        Err(injected()),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");

    let raced = rig.placed("hl-allocation");
    let key = keys::allocation(&raced);
    let row = rig.raw(&key).expect("written").value;
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig.jobs.requeue_held(&raced, held_on_a(&rig), &history);
    assert_eq!(
        (answer.map(drop), state_of(&rig.jobs, &raced)),
        (
            Err(JobStoreError::Stale { id: raced.clone() }),
            Some(JobState::Placed)
        ),
        "{name}: a requeue over a changed allocation was not stale, or requeued the job"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        Some(row),
        "{name}: the other writer's allocation row was written over"
    );
    assert_eq!(
        rig.raw(&format!("usage/ended-id/{raced}")),
        None,
        "{name}: a row beside the free was written"
    );

    let cancelled = rig.placed("hl-job");
    let other = rig.other();
    let target = cancelled.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs
            .requeue_held(&cancelled, held_on_a(&rig), &history)
            .map(drop),
        Err(JobStoreError::Stale {
            id: cancelled.clone()
        }),
        "{name}: a requeue over a changed job was not stale"
    );
    let job = rig.jobs.get_job(&cancelled).expect("reads");
    assert_eq!(
        job.map(|job| (job.state, job.cancel_requested, job.held_refusals)),
        Some((JobState::Placed, true, Vec::new())),
        "{name}: the other writer's cancel was lost or the refusal written"
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

/// A job whose start `host-a` refused for capacity goes back to queued as after a held refusal
/// (its submit time kept, its allocation freed with `Refused` at once, its start gone from the
/// dispatch queue with no stop, the bars and the note given, no failed prepare counted) but
/// the host is appended to its capacity refusals and not to its held refusals. A later held
/// refusal on another host is appended to the held refusals and leaves the capacity refusals
/// as they were, and a second capacity refusal follows the first in order. Catches the host
/// written to the wrong list, and one list overwriting the other.
pub fn a_capacity_refusal_requeues_the_job_and_names_the_host_in_its_own_list<B: Backend>(
    backend: &B,
) {
    let name = "a_capacity_refusal_requeues_the_job_and_names_the_host_in_its_own_list";
    let rig = rig(backend);
    let id = rig.placed("cr-first");
    rig.tick();
    let placed = rig.jobs.get_job(&id).expect("reads").expect("recorded");
    let first = rig
        .jobs
        .get_allocation(&id)
        .expect("reads")
        .expect("booked");
    let now = rig.store.now().expect("the store's clock");
    let requeue = HeldRequeue {
        refused: RequeuedFor::Capacity,
        ..held_on_a(&rig)
    };
    let (queued, freed) = rig
        .jobs
        .requeue_held(&id, requeue.clone(), &history)
        .expect("a refused start requeues");
    let mut trail = placed.queue.clone();
    trail.apply_all(requeue.queue.as_slice(), now);
    assert_eq!(
        queued,
        JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            prepare_bars: requeue.prepare_bars.clone(),
            capacity_refusals: vec![host("host-a")],
            queue: trail,
            ..placed
        },
        "{name}: the job is not queued with the host in its capacity refusals, and nothing else"
    );
    assert_eq!(
        freed,
        AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::Refused),
            ..first
        },
        "{name}: the allocation is not freed with the refusal's reason"
    );
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [],
        "{name}: the start was left in the dispatch queue or a stop was queued"
    );

    rig.tick();
    rig.jobs
        .place(&id, host("host-b"), share())
        .expect("placed again");
    let held = HeldRequeue {
        prepare_bars: BTreeMap::new(),
        queue: None,
        ..held_on_a(&rig)
    };
    let (after_held, _) = rig
        .jobs
        .requeue_held(&id, held, &|_| Vec::new())
        .expect("a held refusal requeues");
    assert_eq!(
        (after_held.held_refusals, after_held.capacity_refusals),
        (vec![host("host-b")], vec![host("host-a")]),
        "{name}: a held refusal changed the capacity refusals or went to the wrong list"
    );

    rig.tick();
    rig.jobs
        .place(&id, host("host-c"), share())
        .expect("placed again");
    let again = HeldRequeue {
        refused: RequeuedFor::Capacity,
        prepare_bars: BTreeMap::new(),
        queue: None,
    };
    let (twice, _) = rig
        .jobs
        .requeue_held(&id, again, &|_| Vec::new())
        .expect("a second capacity refusal requeues");
    assert_eq!(
        (twice.held_refusals, twice.capacity_refusals),
        (vec![host("host-b")], vec![host("host-a"), host("host-c")]),
        "{name}: the second capacity refusal did not follow the first"
    );
}

/// A job whose start `host-a` refused while it fetches the job's bundle goes back to queued as
/// after a held refusal (its allocation freed with `Refused` at once, its start gone from the
/// dispatch queue with no stop, the bars and the note given, no failed prepare counted), and
/// the host is appended to neither its held refusals nor its capacity refusals, which keep
/// what earlier refusals wrote. Catches a bundle-pending refusal counted as a held or a capacity
/// one, and one that clears the lists.
pub fn a_bundle_pending_refusal_requeues_the_job_and_names_the_host_in_no_list<B: Backend>(
    backend: &B,
) {
    let name = "a_bundle_pending_refusal_requeues_the_job_and_names_the_host_in_no_list";
    let rig = rig(backend);
    let id = rig.placed("bp-first");
    rig.tick();
    let capacity = HeldRequeue {
        refused: RequeuedFor::Capacity,
        prepare_bars: BTreeMap::new(),
        queue: None,
    };
    rig.jobs
        .requeue_held(&id, capacity, &|_| Vec::new())
        .expect("a capacity refusal requeues");
    rig.tick();
    rig.jobs
        .place(&id, host("host-a"), share())
        .expect("placed again");
    let placed = rig.jobs.get_job(&id).expect("reads").expect("recorded");
    let booked = rig
        .jobs
        .get_allocation(&id)
        .expect("reads")
        .expect("booked");
    let now = rig.store.now().expect("the store's clock");
    let requeue = HeldRequeue {
        refused: RequeuedFor::BundlePending,
        ..held_on_a(&rig)
    };
    let (queued, freed) = rig
        .jobs
        .requeue_held(&id, requeue.clone(), &history)
        .expect("a bundle-pending refusal requeues");
    let mut trail = placed.queue.clone();
    trail.apply_all(requeue.queue.as_slice(), now);
    assert_eq!(
        queued,
        JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            prepare_bars: requeue.prepare_bars.clone(),
            queue: trail,
            ..placed.clone()
        },
        "{name}: the job is not queued with its lists as they were, and nothing else"
    );
    assert_eq!(
        (queued.held_refusals, queued.capacity_refusals),
        (Vec::new(), vec![host("host-a")]),
        "{name}: the bundle-pending refusal changed a refusal list"
    );
    assert_eq!(
        freed,
        AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::Refused),
            ..booked
        },
        "{name}: the allocation is not freed with the refusal's reason"
    );
    assert_eq!(
        dispatch_of(&rig, "host-a"),
        [],
        "{name}: the start was left in the dispatch queue or a stop was queued"
    );
}
