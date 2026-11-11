//! The mark table: it is additive to a store written without it, a mark round-trips with its
//! exclusions and their ends, only a queued whole-host job holds one, a row this build cannot
//! read is refused or left as the other tables' are, the boot raises a mark's deadline, and
//! [`crate::JobStore::end_and_unmark`] ends a job and deletes its mark both or neither.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreError, StoreTime, Txn};

use super::{Rig, cancelled, exited, host, job_id, rig, whole_host_spec};
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT, field_of, set_field};
use crate::interfere::INJECTED;
use crate::keys;
use crate::{
    BOOT_GRACE, Exclusion, JobEnd, JobId, JobRecord, JobState, JobStore, JobStoreError, MarkRecord,
    RandomIds,
};

/// A tag no build has used, standing for a field a later build adds to a mark row.
const LATER_TAG: u16 = 902;

/// The store's clock now.
pub(super) fn now<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.store.now()
}

/// `secs` seconds after `from`.
pub(super) fn after(from: StoreTime, secs: u64) -> StoreTime {
    from.saturating_add(Duration::from_secs(secs))
}

/// A mark of `id` on `on` made now, with every field set: a tail end, and an exclusion of
/// each kind, two with ends.
fn full_mark<S: Store>(rig: &Rig<S>, id: &JobId, on: &str) -> MarkRecord {
    let made = now(rig);
    MarkRecord {
        tail_end: Some(after(made, 300)),
        exclusions: BTreeMap::from([
            (host("host-x"), Exclusion::Backoff(after(made, 700))),
            (host("host-y"), Exclusion::Until(after(made, 800))),
            (host("host-z"), Exclusion::ForLife),
        ]),
        ..MarkRecord::new(id.clone(), host(on), made, after(made, 600))
    }
}

/// Records the queued whole-host job `text` and puts [`full_mark`] for it on `host-a`.
pub(super) fn marked<S: Store>(rig: &Rig<S>, text: &str) -> (JobId, MarkRecord) {
    let id = rig.queue_spec(text, whole_host_spec());
    let mark = rig
        .jobs
        .put_mark(full_mark(rig, &id, "host-a"))
        .expect("a queued whole-host job takes a mark");
    (id, mark)
}

/// A hook that rewrites the row `key` with the bytes it holds now: the same value at a new
/// version, as any other writer's commit leaves it.
pub(super) fn rewrite<S: Store + 'static>(
    rig: &Rig<S>,
    key: &str,
) -> impl FnOnce() + Send + 'static {
    let store = rig.store.inner();
    let value = rig.raw(key).expect("written").value;
    let key = key.to_owned();
    move || {
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("reachable")
            .expect("commits");
    }
}

/// The state of the job `id` as stored.
pub(super) fn state_of<S: Store>(jobs: &JobStore<S>, id: &JobId) -> Option<JobState> {
    jobs.get_job(id).expect("reads").map(|job| job.state)
}

/// What a reader of the job and allocation tables sees: every job, the queue, and every live
/// allocation.
type View = (Vec<JobRecord>, Vec<JobRecord>, Vec<crate::AllocationRecord>);

fn view<S: Store>(jobs: &JobStore<S>) -> View {
    let all = jobs
        .list_jobs(None, None, NonZeroUsize::MIN.saturating_add(99))
        .expect("lists");
    assert_eq!(all.resume, None, "the view is one page");
    let queued = jobs.queued().expect("reads the queue");
    let live = jobs.live_allocations().expect("reads the allocations");
    (all.jobs, queued, live)
}

/// Every row outside the mark table, as stored.
fn unmarked_rows<S: Store>(rig: &Rig<S>) -> Vec<(String, js_state::Versioned)> {
    let (rows, _) = rig.snapshot();
    rows.into_iter()
        .filter(|(key, _)| !key.starts_with(keys::MARK))
        .collect()
}

/// A store written without marks (jobs queued, placed and ended, allocations booked and
/// releasing) opens with nothing written, holds no mark, lists its jobs and allocations as
/// before, and then accepts a mark, which leaves every job, allocation and index row as it
/// was.
pub fn marks_are_additive_to_an_existing_store<B: Backend>(backend: &B) {
    let name = "marks_are_additive_to_an_existing_store";
    let rig = rig(backend);
    let whole = rig.queue_spec("ad-whole", whole_host_spec());
    rig.placed("ad-placed");
    let ended = rig.running("ad-ended");
    rig.jobs.end_job(&ended, exited(0)).expect("ends");
    let seen = view(&rig.jobs);
    let before = rig.snapshot();
    assert!(
        before.0.iter().all(|(key, _)| !key.starts_with(keys::MARK)),
        "{name}: a store written without marks holds one"
    );

    let reopened = rig.other();
    assert_eq!(rig.snapshot(), before, "{name}: the open wrote");
    assert_eq!(view(&reopened), seen, "{name}: the open reads otherwise");
    assert_eq!(reopened.marks(), Ok(Vec::new()), "{name}: a mark appeared");
    assert_eq!(
        reopened.get_mark(&whole),
        Ok(None),
        "{name}: a mark appeared"
    );

    let rows = unmarked_rows(&rig);
    let mark = MarkRecord::new(
        whole.clone(),
        host("host-a"),
        now(&rig),
        after(now(&rig), 60),
    );
    assert_eq!(
        reopened.put_mark(mark.clone()),
        Ok(mark.clone()),
        "{name}: the store did not accept a mark"
    );
    assert_eq!(reopened.get_mark(&whole), Ok(Some(mark.clone())));
    assert_eq!(reopened.marks(), Ok(vec![mark]));
    assert_eq!(
        unmarked_rows(&rig),
        rows,
        "{name}: the mark wrote a job, allocation or index row"
    );
    assert_eq!(
        view(&reopened),
        seen,
        "{name}: the mark changed what a list reads"
    );
    assert_eq!(
        reopened.index_entries(),
        reopened.index_by_scan(),
        "{name}: the indexes no longer agree with the tables"
    );
}

/// A mark reads back as written, with its host, deadline, `made`, tail end and every
/// exclusion with its end, and so does one with none of the optional fields; the marks list
/// in job id order; a second mark of the same job replaces the first; and every mark survives
/// a restart.
pub fn a_mark_round_trips_with_its_exclusions_and_made<B: Backend>(backend: &B) {
    let name = "a_mark_round_trips_with_its_exclusions_and_made";
    let rig = rig(backend);
    let (full_id, full) = marked(&rig, "rt-1");
    assert_eq!(
        rig.jobs.get_mark(&full_id),
        Ok(Some(full.clone())),
        "{name}: the full mark did not read back as written"
    );
    let bare_id = rig.queue_spec("rt-2", whole_host_spec());
    let bare = MarkRecord::new(bare_id.clone(), host("host-b"), now(&rig), now(&rig));
    assert_eq!(rig.jobs.put_mark(bare.clone()), Ok(bare.clone()));
    assert_eq!(
        rig.jobs.get_mark(&bare_id),
        Ok(Some(bare.clone())),
        "{name}: the bare mark did not read back as written"
    );
    assert_eq!(
        rig.jobs.marks(),
        Ok(vec![full.clone(), bare.clone()]),
        "{name}: the marks are not listed in job id order"
    );

    rig.tick();
    let moved = MarkRecord {
        deadline: after(now(&rig), 900),
        exclusions: BTreeMap::from([(host("host-a"), Exclusion::Backoff(after(now(&rig), 5)))]),
        ..full_mark(&rig, &full_id, "host-c")
    };
    assert_eq!(rig.jobs.put_mark(moved.clone()), Ok(moved.clone()));
    assert_eq!(
        rig.jobs.get_mark(&full_id),
        Ok(Some(moved.clone())),
        "{name}: a second mark did not replace the first"
    );

    let Rig { store, jobs, .. } = rig;
    drop(jobs);
    let store = Arc::into_inner(store)
        .and_then(|wrapped| wrapped.into_inner())
        .expect("nothing else holds the store");
    let jobs = JobStore::new(Arc::new(backend.restart(store)), RandomIds).expect("reopens");
    assert_eq!(
        jobs.marks(),
        Ok(vec![moved, bare]),
        "{name}: the marks did not survive a restart as written"
    );
}

/// A field a later build added to a mark row is kept when the mark is written again on the
/// same host, and not when it moves to another host, which makes a new mark.
pub fn a_mark_keeps_a_later_builds_field_only_on_its_host<B: Backend>(backend: &B) {
    let name = "a_mark_keeps_a_later_builds_field_only_on_its_host";
    let rig = rig(backend);
    let (id, mark) = marked(&rig, "lf-1");
    let key = keys::mark(&id);
    let row = rig.raw(&key).expect("written").value;
    rig.put_raw(&key, set_field(&row, LATER_TAG, &[7, 7]).expect("parses"));
    let later = || field_of(&rig.raw(&key).expect("written").value, LATER_TAG);

    let read = rig.jobs.get_mark(&id).expect("reads").expect("a mark");
    assert_eq!(
        MarkRecord {
            carried: mark.carried.clone(),
            ..read.clone()
        },
        mark,
        "{name}: a row with a later field reads otherwise"
    );
    let again = MarkRecord {
        deadline: after(mark.deadline, 1),
        ..full_mark(&rig, &id, "host-a")
    };
    rig.jobs.put_mark(again).expect("rewrites");
    assert_eq!(
        later(),
        Some(vec![7, 7]),
        "{name}: a rewrite on the same host lost the later field"
    );
    rig.jobs.put_mark(read).expect("rewrites what it read");
    assert_eq!(
        later(),
        Some(vec![7, 7]),
        "{name}: a rewrite of the read mark lost it"
    );
    rig.jobs
        .put_mark(full_mark(&rig, &id, "host-b"))
        .expect("moves");
    assert_eq!(
        later(),
        None,
        "{name}: a mark on another host carried the old mark's field"
    );
}

/// A mark for a job that does not exist, that does not need the whole host, or that is not
/// queued (placed, or ended) is refused with the reason, and nothing is written.
pub fn only_a_queued_whole_host_job_takes_a_mark<B: Backend>(backend: &B) {
    let name = "only_a_queued_whole_host_job_takes_a_mark";
    let rig = rig(backend);
    let share = rig.queue("om-share");
    let placed = rig.queue_spec("om-placed", whole_host_spec());
    rig.jobs
        .place(&placed, host("host-a"), crate::Booked::WholeHost)
        .expect("placed");
    let ended = rig.queue_spec("om-ended", whole_host_spec());
    rig.jobs.end_job(&ended, cancelled()).expect("cancelled");
    let missing = job_id("om-missing");
    let before = rig.snapshot();
    let mark = |id: &JobId| MarkRecord::new(id.clone(), host("host-b"), now(&rig), now(&rig));
    let refused = [
        (
            &missing,
            JobStoreError::NotFound {
                id: missing.clone(),
            },
        ),
        (&share, JobStoreError::NotWholeHost { id: share.clone() }),
        (
            &placed,
            JobStoreError::UnexpectedState {
                id: placed.clone(),
                expected: JobState::Queued,
                found: JobState::Placed,
            },
        ),
        (
            &ended,
            JobStoreError::UnexpectedState {
                id: ended.clone(),
                expected: JobState::Queued,
                found: JobState::Cancelled,
            },
        ),
    ];
    for (id, error) in refused {
        assert_eq!(
            rig.jobs.put_mark(mark(id)),
            Err(error),
            "{name}: a mark for {id} was not refused for its reason"
        );
    }
    assert_eq!(rig.snapshot(), before, "{name}: a refused mark wrote");
}

/// A mark row a later build wrote is unknown to a read and to a mark that would replace it,
/// left out of the list, and left as it is by the boot; a delete and an end with unmark remove
/// it. A row that does not parse, or names another job than its key, is corrupt to a read, to
/// the list and to the boot.
pub fn a_mark_row_this_build_cannot_read_is_refused_or_left<B: Backend>(backend: &B) {
    let name = "a_mark_row_this_build_cannot_read_is_refused_or_left";
    let rig = rig(backend);
    let (good, mark) = marked(&rig, "ur-good");
    let newer = rig.queue_spec("ur-newer", whole_host_spec());
    let newer_key = keys::mark(&newer);
    rig.put_raw(&newer_key, vec![LATER_FORMAT]);
    let unknown = JobStoreError::UnknownRow {
        key: newer_key.clone(),
    };
    assert_eq!(
        rig.jobs.get_mark(&newer),
        Err(unknown.clone()),
        "{name}: a read"
    );
    let before = rig.snapshot();
    let replace = MarkRecord::new(newer.clone(), host("host-b"), now(&rig), now(&rig));
    assert_eq!(
        rig.jobs.put_mark(replace),
        Err(unknown),
        "{name}: a put over it"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refused put wrote");
    assert_eq!(
        rig.jobs.marks(),
        Ok(vec![mark.clone()]),
        "{name}: the list did not leave the later row out"
    );
    let boot = after(now(&rig), 3600);
    rig.jobs.raise_at_boot(boot).expect("raises the rest");
    assert_eq!(
        rig.raw(&newer_key).map(|row| row.value),
        Some(vec![LATER_FORMAT]),
        "{name}: the boot wrote a later build's mark"
    );
    assert_eq!(rig.jobs.delete_mark(&newer), Ok(true), "{name}: a delete");
    assert_eq!(rig.raw(&newer_key), None, "{name}: the delete left the row");

    rig.put_raw(&keys::mark(&good), vec![FORMAT, 0]);
    let corrupt = JobStoreError::Corrupt {
        key: keys::mark(&good),
        why: DecodeError::Truncated,
    };
    assert_eq!(
        rig.jobs.get_mark(&good),
        Err(corrupt.clone()),
        "{name}: a read"
    );
    assert_eq!(rig.jobs.marks(), Err(corrupt.clone()), "{name}: the list");
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Err(corrupt),
        "{name}: the boot"
    );
    let ended = rig.jobs.end_and_unmark(&good, cancelled()).expect("ends");
    assert_eq!(ended.state, JobState::Cancelled);
    assert_eq!(
        rig.raw(&keys::mark(&good)),
        None,
        "{name}: the end left the row"
    );

    let (copied, _) = marked(&rig, "ur-copied");
    let other = rig.queue_spec("ur-other", whole_host_spec());
    let row = rig.raw(&keys::mark(&copied)).expect("written").value;
    rig.put_raw(&keys::mark(&other), row);
    assert_eq!(
        rig.jobs.get_mark(&other),
        Err(JobStoreError::Corrupt {
            key: keys::mark(&other),
            why: DecodeError::WrongKey,
        }),
        "{name}: a row under another job's key was taken for that job's"
    );
}

/// At boot one commit raises a mark whose deadline is before the boot plus [`BOOT_GRACE`] to
/// that instant, with the job floors, and leaves a later deadline, the mark's other fields and
/// its exclusions' ends as they are; a second raise at the same boot writes nothing; a boot
/// that has only a mark to raise commits it; and a raise that loses to a change of a mark is
/// stale, naming its job, and writes nothing.
pub fn marks_are_raised_at_boot<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "marks_are_raised_at_boot";
    let rig = rig(backend);
    let (early, early_mark) = marked(&rig, "bm-early");
    let late = rig.queue_spec("bm-late", whole_host_spec());
    let late_mark = MarkRecord::new(
        late.clone(),
        host("host-b"),
        now(&rig),
        after(now(&rig), 86_400),
    );
    rig.jobs.put_mark(late_mark.clone()).expect("marks");
    rig.clock.advance(Duration::from_secs(3600));
    let boot = now(&rig);
    let floor = boot.saturating_add(BOOT_GRACE);
    let late_row = rig.raw(&keys::mark(&late));
    let commits = rig.store.commit_stats().txns;

    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(3),
        "{name}: the two queued jobs and the early mark were not each written"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the boot raise was not one commit"
    );
    let raised = MarkRecord {
        deadline: floor,
        ..early_mark.clone()
    };
    assert_eq!(
        rig.jobs.get_mark(&early),
        Ok(Some(raised.clone())),
        "{name}: the early deadline is not the boot plus the grace, or more changed"
    );
    assert_eq!(
        rig.raw(&keys::mark(&late)),
        late_row,
        "{name}: a later deadline was written"
    );
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(0),
        "{name}: a second raise wrote"
    );
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: a raise with nothing to write committed"
    );

    let lapsed = MarkRecord {
        deadline: boot,
        ..raised.clone()
    };
    rig.jobs.put_mark(lapsed.clone()).expect("marks");
    let key = keys::mark(&early);
    rig.store.before_next(rewrite(&rig, &key));
    let row = rig.raw(&key).map(|row| row.value);
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Err(JobStoreError::Stale { id: early.clone() }),
        "{name}: a raise over a changed mark was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        row,
        "{name}: the stale raise wrote the mark"
    );
    assert_eq!(
        rig.jobs.raise_at_boot(boot),
        Ok(1),
        "{name}: a boot with only a mark to raise did not write it"
    );
    assert_eq!(rig.jobs.get_mark(&early), Ok(Some(raised)));
}

/// An end with unmark of a marked job ends it and deletes its mark in one commit; a forced
/// failure of the commit, a mark that changed before it, and a mark written before it for a
/// job that had none each leave both as they were. A job that has already ended keeps its
/// first end and loses its mark; one with no mark either writes nothing; an end the job
/// cannot make, and an unknown job, are refused and write nothing.
pub fn end_and_unmark_deletes_both_or_neither<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "end_and_unmark_deletes_both_or_neither";
    let rig = rig(backend);
    let (id, mark) = marked(&rig, "eu-marked");
    let key = keys::mark(&id);
    let row = rig.raw(&key).map(|row| row.value);
    rig.store.before_next(rewrite(&rig, &key));
    let answer = rig.jobs.end_and_unmark(&id, cancelled());
    assert_eq!(
        state_of(&rig.jobs, &id),
        Some(JobState::Queued),
        "{name}: the job ended without its mark's delete"
    );
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: an end over a changed mark was not stale"
    );
    assert_eq!(
        rig.raw(&key).map(|row| row.value),
        row,
        "{name}: the mark was deleted without the end"
    );

    let before = rig.snapshot();
    rig.store.fail_next();
    assert_eq!(
        rig.jobs.end_and_unmark(&id, cancelled()).map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            INJECTED.to_owned()
        ))),
        "{name}: a failed commit did not answer the store's error"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a failed commit wrote");
    assert_eq!(rig.jobs.get_mark(&id), Ok(Some(mark)));

    rig.tick();
    let commits = rig.store.commit_stats().txns;
    let ended = rig.jobs.end_and_unmark(&id, cancelled()).expect("ends");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "{name}: the end and the delete took other than one commit"
    );
    assert_eq!(ended.state, JobState::Cancelled, "{name}: not cancelled");
    assert_eq!(rig.jobs.get_job(&id), Ok(Some(ended.clone())));
    assert_eq!(rig.jobs.get_mark(&id), Ok(None), "{name}: the mark is left");

    let unmarked = rig.queue_spec("eu-unmarked", whole_host_spec());
    let other = rig.other();
    let raced = MarkRecord::new(unmarked.clone(), host("host-b"), now(&rig), now(&rig));
    let put = raced.clone();
    rig.store.before_next(move || {
        other.put_mark(put).expect("the other writer marks");
    });
    assert_eq!(
        rig.jobs.end_and_unmark(&unmarked, cancelled()).map(drop),
        Err(JobStoreError::Stale {
            id: unmarked.clone()
        }),
        "{name}: an end that a new mark raced was not stale"
    );
    assert_eq!(state_of(&rig.jobs, &unmarked), Some(JobState::Queued));
    assert_eq!(rig.jobs.get_mark(&unmarked), Ok(Some(raced)));

    let plain = rig.queue_spec("eu-plain", whole_host_spec());
    let ended_plain = rig.jobs.end_and_unmark(&plain, cancelled()).expect("ends");
    assert_eq!(ended_plain.state, JobState::Cancelled);
    assert_eq!(
        rig.raw(&keys::mark(&plain)),
        None,
        "{name}: a mark was made"
    );

    let (kept, _) = marked(&rig, "eu-kept");
    let first = rig
        .jobs
        .end_job(&kept, cancelled())
        .expect("ends, mark kept");
    rig.tick();
    let commits = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.end_and_unmark(&kept, JobEnd::Lost),
        Ok(first.clone()),
        "{name}: an ended job did not keep its first end"
    );
    assert_eq!(rig.store.commit_stats().txns, commits + 1);
    assert_eq!(
        rig.jobs.get_mark(&kept),
        Ok(None),
        "{name}: its mark is left"
    );

    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.end_and_unmark(&kept, JobEnd::Lost),
        Ok(first),
        "{name}: an ended job without a mark did not answer its record"
    );
    assert_eq!(
        rig.snapshot(),
        before,
        "{name}: an ended job without a mark wrote"
    );

    let wrong = rig.queue_spec("eu-wrong", whole_host_spec());
    rig.jobs
        .put_mark(MarkRecord::new(
            wrong.clone(),
            host("host-c"),
            now(&rig),
            now(&rig),
        ))
        .expect("marks");
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.end_and_unmark(&wrong, exited(0)).map(drop),
        Err(JobStoreError::IllegalTransition {
            id: wrong.clone(),
            from: JobState::Queued,
            to: JobState::Succeeded,
        }),
        "{name}: an end a queued job cannot make was not refused"
    );
    let missing = job_id("eu-missing");
    assert_eq!(
        rig.jobs.end_and_unmark(&missing, cancelled()).map(drop),
        Err(JobStoreError::NotFound { id: missing }),
        "{name}: an unknown job was not refused"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a refused end wrote");
}

/// A delete removes a mark at the version read and says it did; with no mark it writes
/// nothing; a mark that changed before its commit is stale and kept.
pub fn delete_mark_removes_the_mark_it_read<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "delete_mark_removes_the_mark_it_read";
    let rig = rig(backend);
    let (id, _) = marked(&rig, "dm-1");
    let key = keys::mark(&id);
    rig.store.before_next(rewrite(&rig, &key));
    let row = rig.raw(&key).map(|row| row.value);
    assert_eq!(
        rig.jobs.delete_mark(&id),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: a delete over a changed mark was not stale"
    );
    assert_eq!(rig.raw(&key).map(|row| row.value), row, "{name}: deleted");
    assert_eq!(rig.jobs.delete_mark(&id), Ok(true), "{name}: not deleted");
    assert_eq!(rig.jobs.get_mark(&id), Ok(None));
    let before = rig.snapshot();
    assert_eq!(
        rig.jobs.delete_mark(&id),
        Ok(false),
        "{name}: a second delete"
    );
    assert_eq!(rig.snapshot(), before, "{name}: a delete of nothing wrote");
    assert_eq!(state_of(&rig.jobs, &id), Some(JobState::Queued));
}

/// A mark whose job ended, or whose row another mark took, between its reads and its commit
/// is stale and writes nothing: no mark is left on a job that has ended.
pub fn a_mark_that_loses_to_an_end_or_another_mark_is_stale<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_mark_that_loses_to_an_end_or_another_mark_is_stale";
    let rig = rig(backend);
    let ending = rig.queue_spec("ms-ending", whole_host_spec());
    let other = rig.other();
    let target = ending.clone();
    rig.store.before_next(move || {
        other
            .end_job(&target, cancelled())
            .expect("the other writer cancels");
    });
    let mark = MarkRecord::new(ending.clone(), host("host-a"), now(&rig), now(&rig));
    assert_eq!(
        rig.jobs.put_mark(mark),
        Err(JobStoreError::Stale { id: ending.clone() }),
        "{name}: a mark over an ended job was not stale"
    );
    assert_eq!(
        rig.raw(&keys::mark(&ending)),
        None,
        "{name}: a mark was left"
    );
    assert_eq!(state_of(&rig.jobs, &ending), Some(JobState::Cancelled));

    let taken = rig.queue_spec("ms-taken", whole_host_spec());
    let other = rig.other();
    let first = MarkRecord::new(taken.clone(), host("host-b"), now(&rig), now(&rig));
    let put = first.clone();
    rig.store.before_next(move || {
        other.put_mark(put).expect("the other writer marks");
    });
    let second = MarkRecord::new(taken.clone(), host("host-c"), now(&rig), now(&rig));
    assert_eq!(
        rig.jobs.put_mark(second.clone()),
        Err(JobStoreError::Stale { id: taken.clone() }),
        "{name}: a mark over a new mark was not stale"
    );
    assert_eq!(rig.jobs.get_mark(&taken), Ok(Some(first)));

    let key = keys::mark(&taken);
    rig.store.before_next(rewrite(&rig, &key));
    assert_eq!(
        rig.jobs.put_mark(second),
        Err(JobStoreError::Stale { id: taken.clone() }),
        "{name}: a mark over a changed mark was not stale"
    );
}
