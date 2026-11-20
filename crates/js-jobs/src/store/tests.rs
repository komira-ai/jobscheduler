//! Tests of the job store's own plumbing: the schema row.

use std::sync::Arc;

use js_state::fake::Fake;
use js_state::{Cond, ManualClock, Store, Txn, Version, Versioned};

use super::{JobStore, SCHEMA_VERSION, schema_of};
use crate::codec::DecodeError;
use crate::error::JobStoreError;
use crate::ids::RandomIds;
use crate::keys;

fn fake() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        js_state::fake::Flaw::None,
    ))
}

// Catches a store that writes the schema row again over a later build's, or refuses a
// version above its own.
#[test]
fn the_schema_row_is_written_once_and_a_higher_version_is_accepted() {
    let store = fake();
    let jobs = JobStore::new(store.clone(), RandomIds).expect("opens");
    assert_eq!(jobs.stored_schema(), SCHEMA_VERSION);
    let row = store.get(keys::SCHEMA).expect("reads").expect("written");
    assert_eq!(row.value, SCHEMA_VERSION.to_be_bytes());

    store
        .txn(Txn::new().put(keys::SCHEMA, 7u32.to_be_bytes().to_vec(), Cond::Any))
        .expect("reachable")
        .expect("commits");
    let later = JobStore::new(store.clone(), RandomIds).expect("a higher version opens");
    assert_eq!(later.stored_schema(), 7);
    let row = store.get(keys::SCHEMA).expect("reads").expect("kept");
    assert_eq!(
        row.value,
        7u32.to_be_bytes(),
        "the later build's row is kept"
    );
}

// Catches a schema row of another width read as some version rather than refused.
#[test]
fn a_schema_row_that_is_not_four_bytes_is_corrupt() {
    let store = fake();
    store
        .txn(Txn::new().put(keys::SCHEMA, vec![1u8, 2, 3], Cond::Any))
        .expect("reachable")
        .expect("commits");
    let refused = JobStore::new(store, RandomIds).err();
    assert_eq!(
        refused,
        Some(JobStoreError::Corrupt {
            key: keys::SCHEMA.to_owned(),
            why: DecodeError::BadLength { tag: 0 },
        })
    );
    let missing = schema_of(None);
    assert_eq!(
        missing,
        Err(JobStoreError::Corrupt {
            key: keys::SCHEMA.to_owned(),
            why: DecodeError::Missing { tag: 0 },
        })
    );
    let read = schema_of(Some(Versioned {
        value: 3u32.to_be_bytes().to_vec(),
        version: Version(1),
        expires_at: None,
    }));
    assert_eq!(read, Ok(3));
}

/// A source whose every draw fails.
struct Broken;

impl crate::ids::IdSource for Broken {
    fn mint(&self) -> Result<[u8; 16], crate::ids::MintError> {
        Err(crate::ids::MintError {
            code: std::num::NonZeroU32::MIN,
        })
    }
}

// Catches a failed draw turned into an id (all zeros, say), or one that leaves a row behind.
#[test]
fn a_source_that_cannot_mint_is_an_error_and_writes_nothing() {
    let store = fake();
    let jobs = JobStore::new(store.clone(), Broken).expect("opens");
    let failed = Err(JobStoreError::Mint(crate::ids::MintError {
        code: std::num::NonZeroU32::MIN,
    }));
    let job = |id| crate::NewJob {
        id,
        spec: crate::suite::full_spec(),
        priority: 0,
        client: None,
    };
    assert_eq!(jobs.insert_queued(job(None)).map(drop), failed);
    let id = crate::JobId::parse("a").expect("an id");
    jobs.insert_queued(job(Some(id.clone())))
        .expect("a job with an id mints nothing");
    assert_eq!(
        jobs.place(&id, crate::suite::host("host-a"), crate::suite::share())
            .map(drop),
        failed
    );
    assert_eq!(
        store.scan_prefix(keys::ALLOCATION).expect("reads"),
        Vec::new()
    );
    assert_eq!(store.scan_prefix(keys::JOB).expect("reads").len(), 1);
}

/// A log sink a test reads back.
#[derive(Clone, Default)]
pub(super) struct Lines(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

// Catches a row a later build wrote that the retention tick passes over without a word,
// or deletes: the tick deletes nothing, keeps the row, and the warning names its key.
#[test]
fn a_tick_logs_each_row_it_leaves() {
    use std::io::Write as _;

    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let store = fake();
    store
        .txn(Txn::new().put("job/later", vec![crate::codec::LATER_FORMAT], Cond::Any))
        .expect("reachable")
        .expect("commits");
    let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
    let deleted = tracing::subscriber::with_default(subscriber, || jobs.tick());
    assert_eq!(deleted, Ok(Vec::new()));
    assert!(store.get("job/later").expect("reads").is_some());
    lines.clone().flush().expect("flushes");
    let said = String::from_utf8(lines.0.lock().expect("unpoisoned").clone()).expect("text");
    assert!(
        said.contains("a job row a later build wrote is left to its retention")
            & said.contains("key=job/later"),
        "{said}"
    );
}

// Catches a row a later build wrote that a list leaves out without a word: the warning
// names the row's key.
#[test]
fn a_list_logs_each_row_it_skips() {
    use std::io::Write as _;

    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let store = fake();
    store
        .txn(Txn::new().put("job/later", vec![crate::codec::LATER_FORMAT], Cond::Any))
        .expect("reachable")
        .expect("commits");
    let jobs = JobStore::new(store, RandomIds).expect("opens");
    let page = tracing::subscriber::with_default(subscriber, || {
        jobs.list_jobs(None, None, std::num::NonZeroUsize::MIN)
            .expect("lists")
    });
    assert_eq!(page.skipped, ["job/later"]);
    lines.clone().flush().expect("flushes");
    let said = String::from_utf8(lines.0.lock().expect("unpoisoned").clone()).expect("text");
    assert!(
        said.contains("a job row a later build wrote is left out of the list")
            & said.contains("key=job/later"),
        "{said}"
    );
}
