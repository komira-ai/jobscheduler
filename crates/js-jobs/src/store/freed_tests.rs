//! What a read of the freed runs ([`JobStore::freed_page`], [`JobStore::freed_page_oldest_first`])
//! returns and asks the store for: the freed entries newest (or oldest) first a page at a time, the allocation and job rows they point at
//! [`SWEEP_BATCH`] keys to a read, and the entries it passes over. Counted on a recording store
//! over the in-memory fake, every clock the test's.

use std::sync::Arc;
use std::time::Duration;

use js_state::fake::{Fake, Flaw};
use js_state::recording::{Read, RecordingStore};
use js_state::{Cond, ManualClock, Store, Txn};

use super::FreedPage;
use crate::codec::{DecodeError, FORMAT, LATER_FORMAT};
use crate::result::{JobEnd, Output, SWEEP_BATCH};
use crate::suite::{full_spec, host, share};
use crate::{EndReason, JobId, JobStore, JobStoreError, MAX_PAGE, NewJob, RandomIds, keys};

/// A job store over a recording store over the fake, and the fake's clock.
type Rig = (
    JobStore<RecordingStore, RandomIds>,
    Arc<RecordingStore>,
    Arc<ManualClock>,
);

/// An empty job store, recorded.
fn rig() -> Rig {
    let clock = Arc::new(ManualClock::default());
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock.clone(), Flaw::None));
    let recording = Arc::new(RecordingStore::new(fake));
    let jobs = JobStore::new(Arc::clone(&recording), RandomIds).expect("the job store opens");
    (jobs, recording, clock)
}

/// Records the job `id`, places it, and cancels and frees it a millisecond after the last
/// free, so that each job's free sorts after the one before.
fn freed(rig: &Rig, id: &str) -> JobId {
    let (jobs, _, clock) = rig;
    let id = JobId::parse(id).expect("an id");
    let new = NewJob {
        id: Some(id.clone()),
        spec: full_spec(),
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).expect("queued");
    jobs.place(&id, host("host-a"), share()).expect("placed");
    clock.advance(Duration::from_millis(1));
    let end = JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    };
    let done = jobs.end_and_free(&id, end, EndReason::Cancelled);
    done.expect("freed");
    id
}

/// The ids of the runs of `page`, in the order it holds them.
fn ids(page: &FreedPage) -> Vec<&str> {
    page.runs.iter().map(|(job, _)| job.id.as_str()).collect()
}

/// Puts `bytes` under `key` as they are: a row this build did not write.
fn put_raw(store: &RecordingStore, key: &str, bytes: Vec<u8>) {
    let txn = Txn::new().put(key, bytes, Cond::Any);
    let written = store.apply_batch(vec![txn]).expect("reachable");
    written.into_iter().for_each(|result| {
        result.expect("written");
    });
}

/// Asserts that a read of 1,200 freed runs pages them newest free first: a first page of the
/// 1,000 freed last ([`MAX_PAGE`]) and a second of the 200 freed first, whatever the order of
/// their ids, each with what its allocation booked; and that each page is one reverse scan of
/// the freed entries and then the allocation rows and the job rows in reads of [`SWEEP_BATCH`]
/// keys, never a row at a time. Catches a read in id order (which cannot stop at the newest
/// runs), and the get of an allocation per job that the read of the ended jobs made before, a
/// round trip each on Postgres.
#[test]
fn the_freed_runs_are_read_newest_first_a_page_at_a_time_in_batched_reads() {
    let rig = rig();
    // Ids that sort against the order of the frees.
    let ids_freed: Vec<JobId> = (0..1200)
        .map(|n| freed(&rig, &format!("f-{:04}", 1199 - n)))
        .collect();
    let (jobs, recording, _) = &rig;
    recording.clear();
    let first = jobs.freed_page(None, MAX_PAGE).expect("a page");
    let resume = first.resume.as_ref();
    let second = jobs.freed_page(resume, MAX_PAGE).expect("a page");
    let mut newest: Vec<&str> = ids_freed[200..].iter().map(JobId::as_str).collect();
    let mut oldest: Vec<&str> = ids_freed[..200].iter().map(JobId::as_str).collect();
    newest.sort_unstable();
    oldest.sort_unstable();
    assert_eq!((ids(&first), first.entries), (newest, 1000));
    assert_eq!((ids(&second), second.entries), (oldest, 200));
    assert_eq!(second.resume, None, "the oldest free ends the read");
    assert!(first.runs.iter().all(|(_, booked)| *booked == share()));
    let reads = recording.reads();
    let scans = reads
        .iter()
        .filter(|read| matches!(read, Read::ScanRange(prefix) if prefix == keys::FREED))
        .count();
    let batches: Vec<usize> = reads
        .iter()
        .filter_map(|read| match read {
            Read::GetMany(keys) => Some(keys.len()),
            _ => None,
        })
        .collect();
    let half = SWEEP_BATCH.get();
    assert_eq!(scans, 2, "{reads:?}");
    assert_eq!(batches, [half, half, half, half, 200, 200]);
    assert_eq!(reads.len(), 2 + 6, "no other read, and no get of one row");
}

/// Asserts that a read of 1,200 freed runs oldest free first pages them the other way: a first
/// page of the 1,000 freed first and a second of the 200 freed last, whatever the order of their
/// ids, each one scan of the freed entries forward; and that the read ends after the newest.
/// Catches an oldest-first read that walks newest first, one whose resume goes back toward older
/// frees (it would read the first page again), and one that never ends.
#[test]
fn the_freed_runs_are_read_oldest_first_when_asked() {
    let rig = rig();
    let ids_freed: Vec<JobId> = (0..1200)
        .map(|n| freed(&rig, &format!("f-{:04}", 1199 - n)))
        .collect();
    let (jobs, recording, _) = &rig;
    recording.clear();
    let first = jobs
        .freed_page_oldest_first(None, MAX_PAGE)
        .expect("a page");
    let resume = first.resume.as_ref();
    let second = jobs
        .freed_page_oldest_first(resume, MAX_PAGE)
        .expect("a page");
    let mut oldest: Vec<&str> = ids_freed[..1000].iter().map(JobId::as_str).collect();
    let mut newest: Vec<&str> = ids_freed[1000..].iter().map(JobId::as_str).collect();
    oldest.sort_unstable();
    newest.sort_unstable();
    assert_eq!((ids(&first), first.entries), (oldest, 1000));
    assert_eq!((ids(&second), second.entries), (newest, 200));
    assert_eq!(second.resume, None, "the newest free ends the read");
    let scans = recording
        .reads()
        .iter()
        .filter(|read| matches!(read, Read::ScanRange(prefix) if prefix == keys::FREED))
        .count();
    assert_eq!(scans, 2);
}

/// Asserts that a page passes over the freed entries it cannot teach from and keeps the rest:
/// an entry whose job row is gone (its retention ended before its allocation's), one whose job
/// row or allocation row a later build wrote (logged), and keeps the one run left, while the
/// page still counts all four entries. Catches a read that stops, or answers an error, at a row
/// the store no longer holds or a later build wrote.
#[test]
fn a_freed_entry_whose_row_is_gone_or_a_later_build_wrote_is_passed_over() {
    let rig = rig();
    let kept = freed(&rig, "kept");
    let gone = freed(&rig, "gone");
    let later_job = freed(&rig, "later-job");
    let later_alloc = freed(&rig, "later-alloc");
    let (jobs, recording, _) = &rig;
    let txn = Txn::new().delete(keys::job(&gone), Cond::Any);
    recording.apply_batch(vec![txn]).expect("reachable");
    put_raw(recording, &keys::job(&later_job), vec![LATER_FORMAT]);
    put_raw(
        recording,
        &keys::allocation(&later_alloc),
        vec![LATER_FORMAT],
    );
    let page = jobs.freed_page(None, MAX_PAGE).expect("a page");
    assert_eq!((ids(&page), page.entries), (vec![kept.as_str()], 4));
}

/// Asserts that a freed row that does not parse, an allocation row or a job row, is the read's
/// error, naming the row: the history is not taught from a part of a page. Catches a row that
/// does not parse passed over as if a later build wrote it.
#[test]
fn a_freed_row_that_does_not_parse_is_the_reads_error() {
    for key in [keys::allocation, keys::job] {
        let rig = rig();
        let id = freed(&rig, "broken");
        let (jobs, recording, _) = &rig;
        put_raw(recording, &key(&id), vec![FORMAT, 0]);
        assert_eq!(
            jobs.freed_page(None, MAX_PAGE),
            Err(JobStoreError::Corrupt {
                key: key(&id),
                why: DecodeError::Truncated,
            })
        );
    }
}
