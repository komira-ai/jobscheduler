//! What a read of the queue asks the store for: the queued space a page at a time, and the job
//! rows its entries point at [`SWEEP_BATCH`] keys to a read, counted on a recording store over
//! the in-memory fake.

use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::recording::{Read, RecordingStore};
use js_state::{ManualClock, Store};

use crate::result::SWEEP_BATCH;
use crate::suite::full_spec;
use crate::{JobId, JobStore, NewJob, RandomIds, keys};

/// Asserts that reading a queue of 1,200 jobs, as every pass that reads the tables does, reads
/// the queued space in three pages of [`SWEEP_BATCH`] entries (and the space of rows this build
/// cannot read in one), and the 1,200 job rows in three batched reads of 500, 500 and 200 keys,
/// with no read of a single row. Before, each row was a get of its own: 1,200 round trips on
/// Postgres. It catches a read of the rows one by one, and batches that lose or repeat a row.
#[test]
fn a_queue_of_1200_jobs_is_read_in_three_pages_and_three_batched_reads() {
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let recording = Arc::new(RecordingStore::new(fake));
    let jobs = JobStore::new(Arc::clone(&recording), RandomIds).expect("the job store opens");
    let ids: Vec<JobId> = (0..1200)
        .map(|n| JobId::parse(&format!("q-{n:04}")).expect("an id"))
        .collect();
    for id in &ids {
        let new = NewJob {
            id: Some(id.clone()),
            spec: full_spec(),
            priority: 0,
            client: None,
        };
        jobs.insert_queued(new).expect("queued");
    }
    recording.clear();
    let queued = jobs.queued().expect("the queue reads");
    let read: Vec<&JobId> = queued.iter().map(|job| &job.id).collect();
    assert_eq!(
        read,
        ids.iter().collect::<Vec<_>>(),
        "a job lost or repeated"
    );

    let reads = recording.reads();
    let pages = |prefix: &str| {
        let of = |read: &&Read| matches!(read, Read::ScanRange(scanned) if scanned == prefix);
        reads.iter().filter(of).count()
    };
    let batches: Vec<usize> = reads
        .iter()
        .filter_map(|read| match read {
            Read::GetMany(keys) => Some(keys.len()),
            _ => None,
        })
        .collect();
    let gets = reads
        .iter()
        .filter(|read| matches!(read, Read::Get(_)))
        .count();
    assert_eq!(gets, 0, "rows read one by one");
    assert_eq!(batches, [SWEEP_BATCH.get(), SWEEP_BATCH.get(), 200]);
    assert_eq!((pages(keys::QUEUED), pages(keys::UNREAD_JOB)), (3, 1));
    assert_eq!(reads.len(), 3 + 1 + 3, "{reads:?}");
}
