//! A placement that writes its prepare deadline and wall limit and drops the job's mark
//! ([`crate::JobStore::place_with`]), and the cancel of a queued job, which drops its mark
//! ([`crate::JobStore::request_cancel`]): each is one commit, writes nothing when its commit
//! fails, and is stale when a row it read changed. The helpers the checks of `prepare` share are
//! here too.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Store, StoreError, StoreTime};

use super::marks::{marked, rewrite, state_of};
use super::{Rig, cancelled, host, rig, whole_host_spec};
use crate::interfere::INJECTED;
use crate::keys;
use crate::{
    AllocationRecord, Booked, Exclusion, FailedPrepare, JobId, JobRecord, JobState, JobStoreError,
    MarkRecord, Placement,
};

/// How long the placements of these checks give the agent to report the host ready.
const PREPARE: Duration = Duration::from_secs(600);

/// The error an operation answers when its commit fails.
pub(super) fn injected() -> JobStoreError {
    JobStoreError::Store(StoreError::Failed(INJECTED.to_owned()))
}

/// The store's clock `secs` seconds on.
pub(super) fn later<S: Store>(rig: &Rig<S>, secs: u64) -> StoreTime {
    rig.store.now().saturating_add(Duration::from_secs(secs))
}

/// The placement the engine makes of a whole-host job: [`PREPARE`] to be ready, no wall limit
/// of its own.
pub(super) fn engine_placement() -> Placement {
    Placement {
        prepare_timeout: Some(PREPARE),
        wall_limit_secs: None,
    }
}

/// Records the whole-host job `text` and places it on `host-a` with [`engine_placement`].
pub(super) fn placed_whole<S: Store>(
    rig: &Rig<S>,
    text: &str,
) -> (JobId, JobRecord, AllocationRecord) {
    let id = rig.queue_spec(text, whole_host_spec());
    let (job, allocation) = rig
        .jobs
        .place_with(&id, host("host-a"), Booked::WholeHost, engine_placement())
        .expect("a queued whole-host job is placed");
    (id, job, allocation)
}

/// A failed prepare that sends the job back to queued: `count` failures so far, the host barred
/// with a back-off and another for life.
pub(super) fn requeue_with<S: Store>(rig: &Rig<S>, count: u32) -> FailedPrepare {
    FailedPrepare {
        failed_prepares: count,
        prepare_bars: BTreeMap::from([
            (host("host-a"), Exclusion::Backoff(later(rig, 300))),
            (host("host-x"), Exclusion::ForLife),
        ]),
        terminal: false,
    }
}

/// The rows given beside a free: one per allocation, under `usage/ended-id/<job>`.
pub(super) fn history(allocation: &AllocationRecord) -> Vec<crate::Row> {
    vec![(
        format!("usage/ended-id/{}", allocation.job),
        b"ended".to_vec(),
    )]
}

/// The job ids of the queue, in order.
pub(super) fn queue_of<S: Store>(rig: &Rig<S>) -> Vec<JobId> {
    let queued = rig.jobs.queued().expect("reads the queue");
    queued.into_iter().map(|job| job.id).collect()
}

/// A placement that is given a prepare timeout and a wall limit writes the deadline (the
/// placement's time plus the timeout) and the limit on the allocation, and deletes the job's
/// mark, all in one commit; without them it writes neither; [`crate::JobStore::place`] leaves
/// the mark and writes neither field; a failed commit and a refused placement write nothing.
pub fn place_with_writes_the_prepare_deadline_and_drops_the_mark<B: Backend>(backend: &B) {
    let name = "place_with_writes_the_prepare_deadline_and_drops_the_mark";
    let rig = rig(backend);
    let (id, _) = marked(&rig, "pw-marked");
    rig.tick();
    let now = rig.store.now();
    let placement = Placement {
        prepare_timeout: Some(PREPARE),
        wall_limit_secs: Some(1200),
    };
    let commits = rig.store.commit_stats().txns;
    let (job, allocation) = rig
        .jobs
        .place_with(&id, host("host-a"), Booked::WholeHost, placement)
        .expect("a marked job is placed");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the placement and the mark's delete took other than one commit"
    );
    assert_eq!(
        (job.state, job.placed_at),
        (JobState::Placed, Some(now)),
        "{name}: the job is not placed at the store's time"
    );
    assert_eq!(
        (
            allocation.created_at,
            allocation.prepare_deadline,
            allocation.wall_limit_secs
        ),
        (now, Some(now.saturating_add(PREPARE)), Some(1200)),
        "{name}: the deadline is not the placement's time plus the timeout, or the limit is lost"
    );
    assert_eq!(
        rig.jobs.get_mark(&id),
        Ok(None),
        "{name}: the mark of the placed job is left"
    );
    assert_eq!(rig.jobs.get_job(&id), Ok(Some(job)));
    assert_eq!(rig.jobs.get_allocation(&id), Ok(Some(allocation)));

    let plain = rig.queue_spec("pw-plain", whole_host_spec());
    let commits = rig.store.commit_stats().txns;
    let (_, bare) = rig
        .jobs
        .place_with(
            &plain,
            host("host-b"),
            Booked::WholeHost,
            Placement::default(),
        )
        .expect("an unmarked job is placed");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the placement of an unmarked job took other than one commit"
    );
    assert_eq!(
        (bare.prepare_deadline, bare.wall_limit_secs),
        (None, None),
        "{name}: a placement with no timeout and no limit wrote one"
    );
    assert_eq!(
        rig.raw(&keys::mark(&plain)),
        None,
        "{name}: a mark was made"
    );

    let (kept, mark) = marked(&rig, "pw-kept");
    let (_, shared) = rig
        .jobs
        .place(&kept, host("host-c"), Booked::WholeHost)
        .expect("placed, its mark kept");
    assert_eq!(
        (shared.prepare_deadline, shared.wall_limit_secs),
        (None, None),
        "{name}: place wrote a deadline or a limit"
    );
    assert_eq!(
        rig.jobs.get_mark(&kept),
        Ok(Some(mark)),
        "{name}: place touched the mark"
    );

    let (failing, mark) = marked(&rig, "pw-failing");
    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs
            .place_with(&failing, host("host-d"), Booked::WholeHost, placement)
            .map(drop),
        Err(injected()),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");
    assert_eq!(rig.jobs.get_mark(&failing), Ok(Some(mark)));

    assert_eq!(
        rig.jobs
            .place_with(&id, host("host-e"), Booked::WholeHost, placement)
            .map(drop),
        Err(JobStoreError::UnexpectedState {
            id: id.clone(),
            expected: JobState::Queued,
            found: JobState::Placed,
        }),
        "{name}: a placed job was placed again"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refused placement wrote");
}

/// A placement that drops a mark is stale, and writes neither row, when the mark it read
/// changed, when a mark appeared for a job that had none, or when the job changed. A store that
/// applies the job's put and then fails the mark's precondition leaves the job placed with its
/// mark, which this check rejects.
pub fn a_placement_that_drops_a_mark_loses_to_a_change_of_the_job_or_the_mark<B: Backend>(
    backend: &B,
) where
    B::Store: 'static,
{
    let name = "a_placement_that_drops_a_mark_loses_to_a_change_of_the_job_or_the_mark";
    let rig = rig(backend);
    let placement = engine_placement();
    let (rewritten, _) = marked(&rig, "pr-rewritten");
    let key = keys::mark(&rewritten);
    let row = rig.raw(&key).map(|row| row.value);
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig
        .jobs
        .place_with(&rewritten, host("host-a"), Booked::WholeHost, placement);
    assert_eq!(
        state_of(&rig.jobs, &rewritten),
        Some(JobState::Queued),
        "{name}: the job was placed without its mark's delete"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale {
            id: rewritten.clone()
        }),
        "{name}: a placement over a changed mark was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        row,
        "{name}: the mark was deleted without the placement"
    );
    assert_eq!(rig.jobs.get_allocation(&rewritten), Ok(None));

    let plain = rig.queue_spec("pr-plain", whole_host_spec());
    let other = rig.other();
    let raced = MarkRecord::new(
        plain.clone(),
        host("host-b"),
        rig.store.now(),
        rig.store.now(),
    );
    let put = raced.clone();
    rig.store.before_next(move || {
        other.put_mark(put).expect("the other writer marks");
    });
    assert_eq!(
        rig.jobs
            .place_with(&plain, host("host-a"), Booked::WholeHost, placement)
            .map(drop),
        Err(JobStoreError::Stale { id: plain.clone() }),
        "{name}: a placement that a new mark raced was not stale"
    );
    assert_eq!(state_of(&rig.jobs, &plain), Some(JobState::Queued));
    assert_eq!(rig.jobs.get_mark(&plain), Ok(Some(raced)));
    assert_eq!(rig.jobs.get_allocation(&plain), Ok(None));

    let (ended, _) = marked(&rig, "pr-ended");
    let other = rig.other();
    let target = ended.clone();
    rig.store.before_next(move || {
        other
            .end_job(&target, cancelled())
            .expect("the other writer ends it");
    });
    assert_eq!(
        rig.jobs
            .place_with(&ended, host("host-a"), Booked::WholeHost, placement)
            .map(drop),
        Err(JobStoreError::Stale { id: ended.clone() }),
        "{name}: a placement over an ended job was not stale"
    );
    assert_eq!(state_of(&rig.jobs, &ended), Some(JobState::Cancelled));
    assert_eq!(rig.jobs.get_allocation(&ended), Ok(None));
}

/// The cancel of a queued job also deletes its mark, in the same commit: a cancel of a marked
/// job is one commit that ends the job and deletes the mark; of an unmarked job it writes as it
/// did and checks the mark absent; a failed commit writes nothing; and the cancel of a placed
/// job is the intent alone and leaves a mark it finds. A store that applies the job's end and
/// then fails the mark's precondition leaves the job cancelled with its mark, which this check
/// rejects.
pub fn a_cancel_of_a_queued_job_drops_its_mark_in_the_same_commit<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_cancel_of_a_queued_job_drops_its_mark_in_the_same_commit";
    let rig = rig(backend);
    let (rewritten, mark) = marked(&rig, "cm-rewritten");
    let key = keys::mark(&rewritten);
    let row = rig.raw(&key).map(|row| row.value);
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig.jobs.request_cancel(&rewritten);
    assert_eq!(
        state_of(&rig.jobs, &rewritten),
        Some(JobState::Queued),
        "{name}: the job was cancelled without its mark's delete"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale {
            id: rewritten.clone()
        }),
        "{name}: a cancel over a changed mark was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        row,
        "{name}: the mark was deleted without the cancel"
    );

    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.request_cancel(&rewritten).map(drop),
        Err(injected()),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");
    assert_eq!(rig.jobs.get_mark(&rewritten), Ok(Some(mark)));

    rig.tick();
    let commits = rig.store.commit_stats().txns;
    let asked = rig
        .jobs
        .request_cancel(&rewritten)
        .expect("a queued job is cancelled");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the cancel and the mark's delete took other than one commit"
    );
    assert_eq!(
        (asked.state, asked.cancel_requested),
        (JobState::Cancelled, true),
        "{name}: the job is not cancelled with the intent recorded"
    );
    assert_eq!(rig.jobs.get_job(&rewritten), Ok(Some(asked)));
    assert_eq!(
        rig.jobs.get_mark(&rewritten),
        Ok(None),
        "{name}: the mark of the cancelled job is left"
    );

    let plain = rig.queue_spec("cm-plain", whole_host_spec());
    let commits = rig.store.commit_stats().txns;
    let cancelled_plain = rig.jobs.request_cancel(&plain).expect("cancelled");
    assert_eq!(
        (rig.store.commit_stats().txns, cancelled_plain.state),
        (commits + 1, JobState::Cancelled),
        "{name}: the cancel of an unmarked job took other than one commit"
    );
    assert_eq!(
        rig.raw(&keys::mark(&plain)),
        None,
        "{name}: a mark was made"
    );

    let late = rig.queue_spec("cm-late", whole_host_spec());
    let other = rig.other();
    let now = rig.store.now();
    let raced = MarkRecord::new(late.clone(), host("host-b"), now, now);
    let put = raced.clone();
    rig.store.before_next(move || {
        other.put_mark(put).expect("the other writer marks");
    });
    assert_eq!(
        rig.jobs.request_cancel(&late).map(drop),
        Err(JobStoreError::Stale { id: late.clone() }),
        "{name}: a cancel that a new mark raced was not stale"
    );
    assert_eq!(state_of(&rig.jobs, &late), Some(JobState::Queued));
    assert_eq!(rig.jobs.get_mark(&late), Ok(Some(raced)));

    let (placed, _, _) = placed_whole(&rig, "cm-placed");
    let now = rig.store.now();
    let leftover = MarkRecord::new(placed.clone(), host("host-a"), now, now);
    rig.put_raw(
        &keys::mark(&placed),
        crate::codec::encode_mark(&leftover).expect("encodes"),
    );
    let intent = rig.jobs.request_cancel(&placed).expect("an intent");
    assert_eq!(
        (intent.state, intent.cancel_requested),
        (JobState::Placed, true),
        "{name}: the cancel of a placed job is not an intent"
    );
    assert_eq!(
        rig.jobs.get_mark(&placed),
        Ok(Some(leftover)),
        "{name}: the cancel of a placed job touched its mark"
    );
}
