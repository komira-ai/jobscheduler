//! The retention sweep of the attempt rows: a job's rows go before it does, the rows an
//! older build's sweep left without their job go over the ticks, and a page that loses to a
//! change of its job deletes nothing.

use std::time::Duration;

use js_state::conformance::Backend;
use js_state::{Cond, Store, StoreTime, Txn};

use super::{Rig, attempt_end, exited, host, job_id, rig, share};
use crate::codec::{Carried, encode_attempt_row};
use crate::{Attempt, AttemptOutcome, JobId, JobState, JobStoreError, SWEEP_BATCH, keys};

/// Eight days: past the seven days of retention a job keeps its result.
const PAST_A_WEEK: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// The rows one search for orphan attempt rows reads: the sweep's `ORPHAN_PAGE`.
const ORPHAN_PAGE: u32 = crate::store::ORPHAN_PAGE.get() as u32;

/// The row of attempt `number` of a job, as an older or newer build might have left it.
fn attempt_at(number: u32) -> Attempt {
    let at = StoreTime::from_duration(Duration::from_secs(u64::from(number)));
    Attempt {
        number,
        host: host("host-a"),
        queued_at: at,
        placed_at: at,
        started_at: None,
        ended_at: at,
        end: attempt_end(AttemptOutcome::Succeeded, Some(0), None, None),
        retried: false,
        carried: Carried::default(),
    }
}

/// Writes the attempt rows `numbers` of the job `id` past the job store, a hundred to a
/// commit.
fn write_rows<S: Store>(rig: &Rig<S>, id: &JobId, numbers: std::ops::RangeInclusive<u32>) {
    let numbers: Vec<u32> = numbers.collect();
    for chunk in numbers.chunks(100) {
        let txn = chunk.iter().fold(Txn::new(), |txn, number| {
            let row = encode_attempt_row(id, &attempt_at(*number)).expect("a row encodes");
            txn.put(keys::attempt(id, *number), row, Cond::Any)
        });
        rig.store
            .inner()
            .txn(txn)
            .expect("the store is reachable")
            .expect("an unconditional put commits");
    }
}

/// The keys of every attempt row there is, in key order.
fn attempt_keys<S: Store>(rig: &Rig<S>) -> Vec<String> {
    rig.store
        .scan_prefix(keys::ATTEMPT)
        .expect("the store is reachable")
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// The keys of the attempt rows of `id`, in order.
fn rows_of<S: Store>(rig: &Rig<S>, id: &JobId) -> Vec<String> {
    rig.store
        .scan_prefix(&keys::attempt_prefix(id))
        .expect("the store is reachable")
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// The keys of attempts `numbers` of `id`.
fn keys_of(id: &JobId, numbers: std::ops::RangeInclusive<u32>) -> Vec<String> {
    numbers.map(|number| keys::attempt(id, number)).collect()
}

/// Records the job `text`, runs it for `attempts` attempts on a host of its own each, every
/// one but the last retried, and ends it with exit 0: `attempts` attempt rows.
fn ran_and_ended<S: Store>(rig: &Rig<S>, text: &str, attempts: u32) -> JobId {
    let id = rig.queue(text);
    for number in 1..=attempts {
        rig.jobs
            .place(&id, host(&format!("host-{number}")), share())
            .expect("placed");
        rig.jobs
            .transition(&id, JobState::Placed, JobState::Starting)
            .expect("placed to starting");
        rig.jobs
            .transition(&id, JobState::Starting, JobState::Running)
            .expect("starting to running");
        rig.tick();
        if number < attempts {
            let failed = attempt_end(AttemptOutcome::Failed, Some(1), None, None);
            let now = rig.store.now().expect("the clock");
            rig.jobs.retry_txn(&id, failed, now).expect("retried");
            rig.jobs
                .free(&id, crate::EndReason::Exited)
                .expect("the retried attempt's allocation is freed");
            rig.tick();
        }
    }
    rig.jobs.end_job(&id, exited(0)).expect("ends");
    id
}

/// A job that ran three attempts keeps its attempt rows through a tick before its retention
/// end, and a tick after it deletes the job's row and every row of its attempts and nothing
/// of another job that is not due; the sweep of exactly the job by `delete_swept` deletes its
/// rows as well. Catches a sweep that deletes the job and leaves its attempt rows, deletes
/// the rows of a job not yet due, or a `delete_swept` that forgets the rows.
pub fn the_sweep_deletes_a_jobs_attempt_rows_first<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "the_sweep_deletes_a_jobs_attempt_rows_first";
    // (a) the tick.
    let rig = rig(backend);
    let old = ran_and_ended(&rig, "sw-old", 3);
    assert_eq!(
        rows_of(&rig, &old),
        keys_of(&old, 1..=3),
        "{name}: its rows"
    );
    rig.clock.advance(Duration::from_secs(6 * 24 * 60 * 60));
    let young = ran_and_ended(&rig, "sw-young", 2);
    rig.clock.advance(Duration::from_secs(24 * 60 * 60 + 60));
    // The old job is due, the young one is not: only the old job and its rows go.
    let before = attempt_keys(&rig);
    let kept = rig.jobs.tick().expect("ticks");
    assert_eq!(kept, vec![old.clone()], "{name}: the old job is due");
    let after = attempt_keys(&rig);
    let mut want = keys_of(&young, 1..=2);
    want.sort();
    assert_eq!(after, want, "{name}: only the young job keeps rows");
    assert_eq!(before.len(), 5, "{name}: five rows before the sweep");
    assert!(
        rig.raw(&keys::job(&old)).is_none() && rig.raw(&keys::job(&young)).is_some(),
        "{name}: the old job's row is gone and the young job's is kept"
    );

    // A tick before the retention end keeps every row.
    let rig_b = self::rig(backend);
    let held = ran_and_ended(&rig_b, "sw-held", 3);
    rig_b.clock.advance(Duration::from_secs(24 * 60 * 60));
    assert_eq!(rig_b.jobs.tick(), Ok(vec![]), "{name}: nothing due");
    assert_eq!(
        rows_of(&rig_b, &held),
        keys_of(&held, 1..=3),
        "{name}: a job not due keeps its rows"
    );

    // delete_swept deletes the rows too.
    rig_b.clock.advance(PAST_A_WEEK);
    assert_eq!(
        rig_b.jobs.delete_swept(std::slice::from_ref(&held)),
        Ok(vec![held.clone()]),
        "{name}: delete_swept deletes the due job"
    );
    assert_eq!(
        attempt_keys(&rig_b),
        Vec::<String>::new(),
        "{name}: delete_swept leaves no attempt row"
    );

    // (b) more rows than two pages: three commits, the last with the job.
    let rig = self::rig(backend);
    let big = ran_and_ended(&rig, "sw-big", 1);
    let total = u32::try_from(SWEEP_BATCH.get() * 2 + 7).expect("fits");
    write_rows(&rig, &big, 1..=total);
    rig.clock.advance(PAST_A_WEEK);
    let before = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![big.clone()]),
        "{name}: the big job"
    );
    assert_eq!(
        rig.store.commit_stats().txns - before,
        3,
        "{name}: two full pages in commits of their own, then the last with the job"
    );
    assert_eq!(
        (attempt_keys(&rig), rig.raw(&keys::job(&big)).is_none()),
        (Vec::<String>::new(), true),
        "{name}: every row and the job are gone"
    );

    // (c) the second commit fails: the job stays, one page is gone, the next tick finishes.
    let rig = self::rig(backend);
    let big = ran_and_ended(&rig, "sw-cut", 1);
    write_rows(&rig, &big, 1..=total);
    rig.clock.advance(PAST_A_WEEK);
    rig.store.fail_after(1);
    assert_eq!(
        rig.jobs.tick(),
        Err(JobStoreError::Store(js_state::StoreError::Failed(
            crate::interfere::INJECTED.to_owned()
        ))),
        "{name}: the second commit fails"
    );
    let batch = u32::try_from(SWEEP_BATCH.get()).expect("fits");
    assert_eq!(
        (rows_of(&rig, &big), rig.raw(&keys::job(&big)).is_some()),
        (keys_of(&big, batch + 1..=total), true),
        "{name}: one page of rows is gone, the rest and the job stay"
    );
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![big.clone()]),
        "{name}: the next tick"
    );
    assert_eq!(
        (attempt_keys(&rig), rig.raw(&keys::job(&big)).is_none()),
        (Vec::<String>::new(), true),
        "{name}: the job and all its rows are gone"
    );
}

/// Attempt rows whose job's row is absent (written raw, and left by an older build's sweep)
/// are deleted by a tick, the rows of a live job and keys that are not `<id>/<number>` stay,
/// a search over more rows than a page walks them over successive ticks and wraps to the
/// start, a job recorded between the search's read and its commit keeps its rows, and a tick
/// with nothing to delete commits nothing. Catches a search that deletes a live job's rows or
/// a malformed key, never reaches rows past the first page, does not wrap, ignores a job
/// recorded meanwhile, or commits when it finds nothing.
pub fn the_sweep_deletes_orphan_attempt_rows<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "the_sweep_deletes_orphan_attempt_rows";
    let rig = rig(backend);
    let ghost = job_id("or-ghost");
    write_rows(&rig, &ghost, 1..=3);
    let old = ran_and_ended(&rig, "or-old", 2);
    rig.delete_raw(&keys::job(&old));
    let live = ran_and_ended(&rig, "or-live", 2);
    for key in ["attempt/x", "attempt/BAD!ID/0000000001"] {
        rig.put_raw(key, b"left".to_vec());
    }
    assert_eq!(
        attempt_keys(&rig).len(),
        9,
        "{name}: nine rows to begin with"
    );
    let before = rig.store.commit_stats().txns;
    assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: no job is due");
    assert_eq!(
        rig.store.commit_stats().txns - before,
        1,
        "{name}: the orphans go in one commit"
    );
    let mut want = vec![
        "attempt/x".to_owned(),
        "attempt/BAD!ID/0000000001".to_owned(),
    ];
    want.extend(keys_of(&live, 1..=2));
    want.sort();
    assert_eq!(
        attempt_keys(&rig),
        want,
        "{name}: only the live job's rows and the malformed keys stay"
    );
    // Nothing to find: nothing is committed.
    let before = rig.store.commit_stats().txns;
    assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: nothing due");
    assert_eq!(
        rig.store.commit_stats().txns,
        before,
        "{name}: a search that finds no orphan commits nothing"
    );

    // More rows than a page: the search walks them over the ticks and wraps.
    let rig = self::rig(backend);
    let page = ORPHAN_PAGE;
    let ghost = job_id("pg-a");
    let live = rig.queue("pg-live");
    write_rows(&rig, &ghost, 1..=page * 2 + 16);
    write_rows(&rig, &live, 1..=5);
    let rest = |first: u32| keys_of(&ghost, first..=page * 2 + 16);
    let mut stays = keys_of(&live, 1..=5);
    for (tick, first) in [(1, page + 1), (2, page * 2 + 1), (3, page * 2 + 17)] {
        assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: tick {tick}");
        let mut want = if first <= page * 2 + 16 {
            rest(first)
        } else {
            vec![]
        };
        want.extend(stays.clone());
        assert_eq!(
            attempt_keys(&rig),
            want,
            "{name}: after tick {tick} one more page of the ghost's rows is gone"
        );
    }
    // The cursor has reached the end: a new orphan is found from the start.
    write_rows(&rig, &ghost, 1..=1);
    assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: tick 4");
    assert_eq!(
        attempt_keys(&rig),
        std::mem::take(&mut stays),
        "{name}: the search wrapped and found the row written behind it"
    );

    // A job recorded between the read and the commit: nothing is deleted.
    let rig = self::rig(backend);
    let raced = job_id("rc-ghost");
    write_rows(&rig, &raced, 1..=2);
    let other = rig.other();
    let id = raced.clone();
    rig.store.before_next(move || {
        other
            .insert_queued(crate::NewJob {
                id: Some(id),
                spec: super::full_spec(),
                priority: 0,
                client: None,
            })
            .expect("the other server records the job");
    });
    assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: the raced tick");
    assert_eq!(
        (rows_of(&rig, &raced), rig.raw(&keys::job(&raced)).is_some()),
        (keys_of(&raced, 1..=2), true),
        "{name}: a job recorded meanwhile keeps its rows"
    );
    assert_eq!(rig.jobs.tick(), Ok(vec![]), "{name}: the tick after");
    assert_eq!(
        rows_of(&rig, &raced),
        keys_of(&raced, 1..=2),
        "{name}: the rows of a live job stay"
    );
}

/// A due job with more rows than a page whose own row is rewritten (same bytes, a new
/// version) before the first page's commit: the tick answers `Stale` and deletes no attempt
/// row and not the job; the next tick deletes them all. Catches a page commit that does not
/// check the job's row, so that rows go while the job's row moved, and a job left half
/// deleted.
pub fn a_sweep_page_that_loses_to_a_change_of_its_job_deletes_nothing<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_sweep_page_that_loses_to_a_change_of_its_job_deletes_nothing";
    let rig = rig(backend);
    let id = ran_and_ended(&rig, "ls-job", 1);
    let total = u32::try_from(SWEEP_BATCH.get() + 7).expect("fits");
    write_rows(&rig, &id, 1..=total);
    rig.clock.advance(PAST_A_WEEK);
    let inner = rig.store.inner();
    let key = keys::job(&id);
    rig.store.before_next(move || {
        let row = inner.get(&key).expect("reads").expect("present");
        inner
            .txn(Txn::new().put(key, row.value, Cond::Any))
            .expect("reachable")
            .expect("the other server rewrites the job");
    });
    assert_eq!(
        rig.jobs.tick(),
        Err(JobStoreError::Stale { id: id.clone() }),
        "{name}: the page loses"
    );
    assert_eq!(
        (rows_of(&rig, &id), rig.raw(&keys::job(&id)).is_some()),
        (keys_of(&id, 1..=total), true),
        "{name}: no row and not the job is deleted"
    );
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![id.clone()]),
        "{name}: the next tick"
    );
    assert_eq!(
        (attempt_keys(&rig), rig.raw(&keys::job(&id)).is_none()),
        (Vec::<String>::new(), true),
        "{name}: everything is gone"
    );
}

/// Two due jobs whose attempt rows each fit a page but together do not: the first job's rows
/// go into the batch's transaction beside both jobs' deletes, and the second's, which would
/// pass the page, are deleted first in a commit of their own that checks its job; the tick
/// takes those two commits and leaves no row and neither job. Catches a batch transaction that
/// gathers every due job's rows whatever their number, and a split that commits the job before
/// its rows.
pub fn a_sweep_transaction_holds_at_most_a_page_of_attempt_rows<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let name = "a_sweep_transaction_holds_at_most_a_page_of_attempt_rows";
    let rig = rig(backend);
    let rows = u32::try_from(SWEEP_BATCH.get() / 2 + 50).expect("fits");
    let first = ran_and_ended(&rig, "sp-first", 1);
    let second = ran_and_ended(&rig, "sp-second", 1);
    write_rows(&rig, &first, 1..=rows);
    write_rows(&rig, &second, 1..=rows);
    rig.clock.advance(PAST_A_WEEK);
    let before = rig.store.commit_stats().txns;
    assert_eq!(
        rig.jobs.tick(),
        Ok(vec![first.clone(), second.clone()]),
        "{name}: both jobs are swept"
    );
    assert_eq!(
        rig.store.commit_stats().txns - before,
        2,
        "{name}: the second job's rows in a commit of their own, then the batch"
    );
    assert_eq!(
        (
            attempt_keys(&rig),
            rig.raw(&keys::job(&first)).is_none(),
            rig.raw(&keys::job(&second)).is_none()
        ),
        (Vec::<String>::new(), true, true),
        "{name}: every row and both jobs are gone"
    );
}
