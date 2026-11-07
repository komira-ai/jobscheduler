//! The failed prepare of a placed whole-host job as the placement engine decided it
//! ([`crate::JobStore::fail_prepare_with`]) and the restart of a prepare after a hello
//! ([`crate::JobStore::restart_prepare`]). Each is one commit, writes nothing when its commit
//! fails, is refused for the jobs and allocations the requeue refuses, and is stale when a row
//! it read changed.

use std::collections::BTreeMap;

use js_state::conformance::Backend;
use js_state::{Cond, Store, Txn};

use super::marks::{marked, rewrite, state_of};
use super::whole::{history, injected, later, placed_whole, queue_of, requeue_with};
use super::{host, job_id, rig, whole_host_spec};
use crate::codec::{alloc_tag, set_field};
use crate::keys;
use crate::{
    AllocationRecord, AllocationStep, Beside, Booked, EndReason, Exclusion, FailedPrepare,
    HostBooking, JobId, JobRecord, JobState, JobStoreError, Placement, Retention, StartFailure,
};

/// A failed prepare that is not terminal puts the job back to queued with the count and bars
/// it is given (replacing the earlier ones), no host and no placement time, keeping its submit
/// time and so its place in the queue; frees the allocation with `FailedPrepare` and writes the
/// rows given beside it; and drops a mark the job still had; all in one commit. The next
/// placement keeps the count and the bars and gets a new allocation.
pub fn a_failed_prepare_requeues_the_job_with_its_count_and_bars<B: Backend>(backend: &B) {
    let name = "a_failed_prepare_requeues_the_job_with_its_count_and_bars";
    let rig = rig(backend);
    let (id, placed, first) = placed_whole(&rig, "fp-first");
    rig.tick();
    let second = rig.queue_spec("fp-second", whole_host_spec());
    rig.tick();
    let now = rig.store.now();
    let failed = requeue_with(&rig, 1);
    let commits = rig.store.commit_stats().txns;
    let (queued, freed) = rig
        .jobs
        .fail_prepare_with(&id, failed.clone(), &history)
        .expect("a failed prepare requeues");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the failed prepare took other than one commit"
    );
    assert_eq!(
        queued,
        JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            failed_prepares: 1,
            prepare_bars: failed.prepare_bars.clone(),
            ..placed
        },
        "{name}: the job is not queued with the count and bars and nothing else changed"
    );
    assert_eq!(
        freed,
        AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::FailedPrepare),
            ..first.clone()
        },
        "{name}: the allocation is not freed with the failed prepare's reason"
    );
    assert_eq!(rig.jobs.get_job(&id), Ok(Some(queued)));
    assert_eq!(rig.jobs.get_allocation(&id), Ok(Some(freed)));
    let written = rig.raw(&format!("usage/ended-id/{id}"));
    assert_eq!(
        written.map(|row| row.value),
        Some(b"ended".to_vec()),
        "{name}: the row beside the free was not written"
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
    let bare = Placement::default();
    let (again, next) = rig
        .jobs
        .place_with(&id, host("host-b"), Booked::WholeHost, bare)
        .expect("placed again");
    assert_eq!(
        (again.failed_prepares, again.prepare_bars),
        (1, failed.prepare_bars),
        "{name}: a placement lost the count or the bars"
    );
    assert_ne!(next.allocation_id, first.allocation_id);
    assert_eq!(
        (next.prepare_deadline, next.wall_limit_secs),
        (None, None),
        "{name}: the placement kept the earlier placement's deadline"
    );
    let replaced = BTreeMap::from([(host("host-b"), Exclusion::ForLife)]);
    let (twice, _) = rig
        .jobs
        .fail_prepare_with(
            &id,
            FailedPrepare {
                failed_prepares: 2,
                prepare_bars: replaced.clone(),
                terminal: false,
            },
            &|_| Vec::new(),
        )
        .expect("a second failed prepare requeues");
    assert_eq!(
        (twice.failed_prepares, twice.prepare_bars),
        (2, replaced),
        "{name}: the count and bars are not the ones given"
    );

    let (leftover, _) = marked(&rig, "fp-leftover");
    rig.jobs
        .place(&leftover, host("host-c"), Booked::WholeHost)
        .expect("placed, its mark kept");
    assert!(
        rig.jobs.get_mark(&leftover).expect("reads").is_some(),
        "{name}: place dropped the mark"
    );
    let commits = rig.store.commit_stats().txns;
    rig.jobs
        .fail_prepare_with(&leftover, requeue_with(&rig, 1), &|_| Vec::new())
        .expect("requeued");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the failed prepare and the mark's delete took other than one commit"
    );
    assert_eq!(
        rig.jobs.get_mark(&leftover),
        Ok(None),
        "{name}: the mark of the requeued job is left"
    );
}

/// A failed prepare that is terminal ends the job failed to start with `ExclusiveNotReady`, as
/// an end writes it (its end time, retention end and no result), with the count and bars it is
/// given, and frees the allocation with `FailedPrepare`, in one commit; the job leaves the queue
/// and the indexes agree with a scan.
pub fn the_last_failed_prepare_ends_the_job_failed_to_start<B: Backend>(backend: &B) {
    let name = "the_last_failed_prepare_ends_the_job_failed_to_start";
    let rig = rig(backend);
    let (id, placed, first) = placed_whole(&rig, "lp");
    rig.tick();
    let now = rig.store.now();
    let failed = FailedPrepare {
        terminal: true,
        ..requeue_with(&rig, 3)
    };
    let commits = rig.store.commit_stats().txns;
    let (ended, freed) = rig
        .jobs
        .fail_prepare_with(&id, failed.clone(), &history)
        .expect("the last failed prepare ends the job");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the end and the free took other than one commit"
    );
    assert_eq!(
        ended,
        JobRecord {
            state: JobState::FailedToStart(StartFailure::ExclusiveNotReady),
            ended_at: Some(now),
            result: None,
            retention_ends: Some(now.saturating_add(Retention::DEFAULT.window())),
            spec: placed.spec.clone().digested(),
            failed_prepares: 3,
            prepare_bars: failed.prepare_bars,
            ..placed
        },
        "{name}: the job did not end failed to start as an end writes it"
    );
    assert_eq!(
        freed,
        AllocationRecord {
            step: AllocationStep::Freed,
            freed_at: Some(now),
            end_reason: Some(EndReason::FailedPrepare),
            ..first
        },
        "{name}: the allocation is not freed with the failed prepare's reason"
    );
    assert_eq!(rig.jobs.get_job(&id), Ok(Some(ended)));
    assert_eq!(rig.jobs.get_allocation(&id), Ok(Some(freed)));
    assert!(
        rig.raw(&format!("usage/ended-id/{id}")).is_some(),
        "{name}: the row beside the free was not written"
    );
    assert!(
        queue_of(&rig).is_empty(),
        "{name}: the ended job is still queued"
    );
    assert_eq!(
        rig.jobs.index_entries(),
        rig.jobs.index_by_scan(),
        "{name}: the indexes differ from the scan"
    );
}

/// A failed prepare is refused, and writes nothing and asks for no rows, for a job that is not
/// placed, that does not need the whole host, whose cancel was asked for, whose allocation is
/// releasing or freed or missing, and that does not exist; a job that fails its prepare asks for
/// its rows once.
pub fn a_failed_prepare_refuses_what_the_requeue_refuses<B: Backend>(backend: &B) {
    let name = "a_failed_prepare_refuses_what_the_requeue_refuses";
    let rig = rig(backend);
    let queued = rig.queue_spec("fr-queued", whole_host_spec());
    let shared = rig.placed("fr-share");
    let (cancelling, _, _) = placed_whole(&rig, "fr-cancel");
    rig.jobs.request_cancel(&cancelling).expect("an intent");
    let (freed, _, _) = placed_whole(&rig, "fr-freed");
    rig.jobs
        .free(&freed, EndReason::HostAbsent)
        .expect("the host read absent");
    let (releasing, _, _) = placed_whole(&rig, "fr-releasing");
    let key = keys::allocation(&releasing);
    let row = rig.raw(&key).expect("an allocation").value;
    rig.put_raw(
        &key,
        set_field(&row, alloc_tag::STEP, &[1]).expect("the row parses"),
    );
    let (no_row, _, _) = placed_whole(&rig, "fr-no-row");
    rig.delete_raw(&keys::allocation(&no_row));
    let missing = job_id("fr-missing");

    let not_whole = |id: &JobId| JobStoreError::IllegalTransition {
        id: id.clone(),
        from: JobState::Placed,
        to: JobState::Queued,
    };
    let not_booked = |id: &JobId, step| JobStoreError::AllocationNotBooked {
        id: id.clone(),
        step,
    };
    let cases = [
        (
            queued.clone(),
            JobStoreError::UnexpectedState {
                id: queued,
                expected: JobState::Placed,
                found: JobState::Queued,
            },
        ),
        (shared.clone(), not_whole(&shared)),
        (cancelling.clone(), not_whole(&cancelling)),
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
        let failed = FailedPrepare {
            failed_prepares: 1,
            prepare_bars: BTreeMap::new(),
            terminal: false,
        };
        assert_eq!(
            rig.jobs.fail_prepare_with(&id, failed, beside).map(drop),
            Err(refusal),
            "{name}: {id} was not refused as it should be"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a refused prepare wrote");
    assert_eq!(asked.get(), 0, "{name}: a refused prepare asked for rows");

    let (ready, _, _) = placed_whole(&rig, "fr-ready");
    rig.jobs
        .fail_prepare_with(&ready, requeue_with(&rig, 1), beside)
        .expect("a placed whole-host job fails its prepare");
    assert_eq!(
        asked.get(),
        1,
        "{name}: a failed prepare asked for its rows"
    );
}

/// A failed prepare whose commit fails writes nothing; one whose job or allocation another
/// writer changed is stale and writes neither row. A store that applies the job's put and then
/// fails the allocation's precondition leaves the job queued with its allocation still booked,
/// which this check rejects.
pub fn a_failed_prepare_loses_to_a_change_of_either_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_failed_prepare_loses_to_a_change_of_either_row";
    let rig = rig(backend);
    let (failing, _, _) = placed_whole(&rig, "fl-failing");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .fail_prepare_with(&failing, requeue_with(&rig, 1), &history)
            .map(drop),
        Err(injected()),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");

    let (raced, _, _) = placed_whole(&rig, "fl-allocation");
    let key = keys::allocation(&raced);
    let row = rig.raw(&key).expect("written").value;
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig
        .jobs
        .fail_prepare_with(&raced, requeue_with(&rig, 1), &history);
    assert_eq!(
        state_of(&rig.jobs, &raced),
        Some(JobState::Placed),
        "{name}: the job was requeued without its allocation"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: a failed prepare over a changed allocation was not stale"
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

    let (cancelled_job, _, _) = placed_whole(&rig, "fl-job");
    let other = rig.other();
    let target = cancelled_job.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs
            .fail_prepare_with(&cancelled_job, requeue_with(&rig, 1), &history)
            .map(drop),
        Err(JobStoreError::Stale {
            id: cancelled_job.clone()
        }),
        "{name}: a failed prepare over a changed job was not stale"
    );
    let job = rig.jobs.get_job(&cancelled_job).expect("reads");
    assert_eq!(
        job.map(|job| (job.state, job.cancel_requested, job.failed_prepares)),
        Some((JobState::Placed, true, 0)),
        "{name}: the other writer's cancel was lost or the count written"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&cancelled_job)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Booked),
        "{name}: the stale failed prepare freed the allocation"
    );
}

/// A failed prepare drops a mark the job still had, and is stale, writing nothing, when that
/// mark changed or a mark was written for a job that had none. A store that applies the job's
/// and allocation's puts and then fails the mark's precondition leaves the job queued with its
/// mark, which this check rejects.
pub fn a_failed_prepare_drops_its_mark_or_writes_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_failed_prepare_drops_its_mark_or_writes_nothing";
    let rig = rig(backend);
    let (marked_job, _) = marked(&rig, "fm-marked");
    rig.jobs
        .place(&marked_job, host("host-a"), Booked::WholeHost)
        .expect("placed, its mark kept");
    let key = keys::mark(&marked_job);
    let row = rig.raw(&key).map(|row| row.value);
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig
        .jobs
        .fail_prepare_with(&marked_job, requeue_with(&rig, 1), &history);
    assert_eq!(
        state_of(&rig.jobs, &marked_job),
        Some(JobState::Placed),
        "{name}: the job was requeued without its mark's delete"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale {
            id: marked_job.clone()
        }),
        "{name}: a failed prepare over a changed mark was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        row,
        "{name}: the mark was deleted without the failed prepare"
    );

    let (unmarked, _, _) = placed_whole(&rig, "fm-unmarked");
    let inner = rig.store.inner();
    let written = keys::mark(&unmarked);
    rig.store.before_next(move || {
        inner
            .txn(Txn::new().put(written, b"another writer".to_vec(), Cond::Any))
            .expect("reachable")
            .expect("commits");
    });
    assert_eq!(
        rig.jobs
            .fail_prepare_with(&unmarked, requeue_with(&rig, 1), &history)
            .map(drop),
        Err(JobStoreError::Stale {
            id: unmarked.clone()
        }),
        "{name}: a failed prepare that a new mark raced was not stale"
    );
    assert_eq!(
        state_of(&rig.jobs, &unmarked),
        Some(JobState::Placed),
        "{name}: the job was requeued though a mark was written"
    );
    assert_eq!(
        rig.jobs
            .get_allocation(&unmarked)
            .expect("reads")
            .map(|allocation| allocation.step),
        Some(AllocationStep::Booked),
        "{name}: the stale failed prepare freed the allocation"
    );
}

/// The prepare of a placed whole-host job starts again with a new deadline, and nothing else
/// changes (the job row included), in one commit; a second restart moves the deadline again.
/// It is refused for a job that is not placed (a starting one among them), that does not need
/// the whole host, and whose allocation is releasing, freed or missing, and for a job that does
/// not exist; a failed commit writes nothing.
pub fn a_restarted_prepare_moves_only_the_deadline<B: Backend>(backend: &B) {
    let name = "a_restarted_prepare_moves_only_the_deadline";
    let rig = rig(backend);
    let (id, placed, first) = placed_whole(&rig, "rp");
    rig.tick();
    let deadline = later(&rig, 600);
    let commits = rig.store.commit_stats().txns;
    let restarted = rig
        .jobs
        .restart_prepare(&id, deadline)
        .expect("a placed whole-host job restarts its prepare");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the restart took other than one commit"
    );
    assert_eq!(
        restarted,
        AllocationRecord {
            prepare_deadline: Some(deadline),
            ..first
        },
        "{name}: the restart changed more than the deadline"
    );
    assert_eq!(rig.jobs.get_allocation(&id), Ok(Some(restarted.clone())));
    assert_eq!(
        rig.jobs.get_job(&id),
        Ok(Some(placed)),
        "{name}: the restart wrote the job"
    );

    rig.tick();
    let again = later(&rig, 600);
    let moved = rig.jobs.restart_prepare(&id, again).expect("restarts");
    assert_eq!(
        moved,
        AllocationRecord {
            prepare_deadline: Some(again),
            ..restarted
        },
        "{name}: a second restart did not move the deadline"
    );

    let before = rig.snapshot();
    let last = later(&rig, 900);
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.restart_prepare(&id, last).map(drop),
        Err(injected()),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");

    let queued = rig.queue_spec("rp-queued", whole_host_spec());
    let shared = rig.placed("rp-share");
    let (starting, _, _) = placed_whole(&rig, "rp-starting");
    rig.jobs
        .transition(&starting, JobState::Placed, JobState::Starting)
        .expect("starts");
    let (freed, _, _) = placed_whole(&rig, "rp-freed");
    rig.jobs
        .free(&freed, EndReason::HostAbsent)
        .expect("the host read absent");
    let (releasing, _, _) = placed_whole(&rig, "rp-releasing");
    let key = keys::allocation(&releasing);
    let row = rig.raw(&key).expect("an allocation").value;
    rig.put_raw(
        &key,
        set_field(&row, alloc_tag::STEP, &[1]).expect("the row parses"),
    );
    let (no_row, _, _) = placed_whole(&rig, "rp-no-row");
    rig.delete_raw(&keys::allocation(&no_row));
    let missing = job_id("rp-missing");
    let unexpected = |id: &JobId, found| JobStoreError::UnexpectedState {
        id: id.clone(),
        expected: JobState::Placed,
        found,
    };
    let not_booked = |id: &JobId, step| JobStoreError::AllocationNotBooked {
        id: id.clone(),
        step,
    };
    let cases = [
        (queued.clone(), unexpected(&queued, JobState::Queued)),
        (starting.clone(), unexpected(&starting, JobState::Starting)),
        (shared.clone(), JobStoreError::NotWholeHost { id: shared }),
        (freed.clone(), not_booked(&freed, AllocationStep::Freed)),
        (
            releasing.clone(),
            not_booked(&releasing, AllocationStep::Releasing),
        ),
        (no_row.clone(), JobStoreError::NoAllocation { id: no_row }),
        (missing.clone(), JobStoreError::NotFound { id: missing }),
    ];
    let before = rig.snapshot();
    for (id, refusal) in cases {
        assert_eq!(
            rig.jobs.restart_prepare(&id, deadline).map(drop),
            Err(refusal),
            "{name}: {id} was not refused as it should be"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a refused restart wrote");
}

/// A restart whose job or allocation another writer changed is stale, and the deadline stays
/// what it was. A store that applies the allocation's put and then fails the job's precondition
/// moves the deadline of a job that changed, which this check rejects.
pub fn a_restarted_prepare_loses_to_a_change_of_either_row<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_restarted_prepare_loses_to_a_change_of_either_row";
    let rig = rig(backend);
    let (cancelling, _, first) = placed_whole(&rig, "rl-job");
    let other = rig.other();
    let target = cancelling.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    let answer = rig.jobs.restart_prepare(&cancelling, later(&rig, 900));
    assert_eq!(
        rig.jobs
            .get_allocation(&cancelling)
            .expect("reads")
            .map(|allocation| allocation.prepare_deadline),
        Some(first.prepare_deadline),
        "{name}: the deadline moved though the job changed"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale {
            id: cancelling.clone()
        }),
        "{name}: a restart over a changed job was not stale"
    );

    let (raced, _, _) = placed_whole(&rig, "rl-allocation");
    let key = keys::allocation(&raced);
    let row = rig.raw(&key).expect("written").value;
    rig.store.before_next(rewrite(&rig, &key));
    let last = later(&rig, 900);
    assert_eq!(
        rig.jobs.restart_prepare(&raced, last).map(drop),
        Err(JobStoreError::Stale { id: raced.clone() }),
        "{name}: a restart over a changed allocation was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        Some(row),
        "{name}: the other writer's allocation row was written over"
    );
}
