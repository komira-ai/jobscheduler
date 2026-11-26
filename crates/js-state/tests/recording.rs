//! `RecordingStore`: forwards everything, and remembers each write it was asked for.
//!
//! Every test names the defect it catches.

use std::sync::Arc;
use std::time::Duration;

use js_state::fake::{Fake, Flaw};
use js_state::recording::{Read, RecordingStore, Write, WriteKind};
use js_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

/// A store that fails every call with [`refused`].
struct Refusing;

fn refused() -> StoreError {
    StoreError::Failed("refused".to_owned())
}

impl Store for Refusing {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(refused())
    }

    fn get(&self, _key: &str) -> Result<Option<Versioned>, StoreError> {
        Err(refused())
    }

    fn scan_prefix(&self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        Err(refused())
    }

    fn scan_range(&self, _scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        Err(refused())
    }

    fn changes(&self, _after: Revision, _limit: usize) -> Result<Changes, StoreError> {
        Err(refused())
    }

    fn trim_feed(&self, _before: Revision) -> Result<u64, StoreError> {
        Err(refused())
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Err(refused())
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats::default()
    }

    fn now(&self) -> Result<StoreTime, StoreError> {
        Ok(StoreTime::ZERO)
    }
}

fn write(key: &str, kind: WriteKind, committed: bool) -> Write {
    Write {
        key: key.to_owned(),
        kind,
        committed,
    }
}

// Catches a recorder that misses a kind of write or misreports it: puts (with and without
// a time to live), deletes and counters are recorded in order with their kind; a check
// is a precondition, not a write, and is not.
#[test]
fn every_kind_of_write_is_recorded_in_order_and_a_check_is_not() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(
            Txn::new()
                .put("a", "1", Cond::Any)
                .put_ttl("b", "2", Duration::from_secs(5), Cond::Any)
                .check("watched", Cond::Any)
                .delete("a", Cond::Any)
                .next("n"),
        )
        .expect("store")
        .expect("commit");
    assert_eq!(
        recording.writes(),
        [
            write("a", WriteKind::Put, true),
            write("b", WriteKind::Put, true),
            write("a", WriteKind::Delete, true),
            write("n", WriteKind::Next, true),
        ]
    );
    assert_eq!(
        recording.keys().into_iter().collect::<Vec<_>>(),
        ["a", "b", "n"]
    );
}

// Catches a recorder that forgets what was refused: a write whose precondition fails is
// recorded as not committed (the keys a component tried to write are what a test needs),
// and in a batch each transaction has its own outcome.
#[test]
fn a_refused_write_is_recorded_as_not_committed() {
    let recording = RecordingStore::new(fake());
    let results = recording
        .apply_batch(vec![
            Txn::new().put("k", "v", Cond::Absent),
            Txn::new().put("k", "w", Cond::Absent),
            Txn::new().put("z", "x", Cond::Any),
        ])
        .expect("store");
    assert!(results[0].is_ok() && results[1].is_err() && results[2].is_ok());
    assert_eq!(
        recording.writes(),
        [
            write("k", WriteKind::Put, true),
            write("k", WriteKind::Put, false),
            write("z", WriteKind::Put, true),
        ]
    );
}

/// Asserts that a store that fails the whole call still has the write recorded, as not
/// committed, and that the error reaches the caller unchanged.
#[test]
fn a_write_the_store_fails_is_recorded_and_the_error_passes_through() {
    let recording = RecordingStore::new(Arc::new(Refusing));
    let err = recording
        .txn(Txn::new().put("mine", "v", Cond::Any))
        .unwrap_err();
    assert_eq!(err, refused());
    assert_eq!(recording.writes(), [write("mine", WriteKind::Put, false)]);
}

/// Asserts that a reap is counted, and that `clear` resets the writes and the reaps to none.
#[test]
fn reaps_are_counted_and_clear_forgets_everything() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("store")
        .expect("commit");
    assert_eq!(recording.reap_expired(), Ok(0));
    assert_eq!(recording.reaps(), 1);
    recording.clear();
    assert_eq!((recording.reaps(), recording.writes()), (0, Vec::new()));
}

/// Asserts that reads, the feed, the clock and the counters answer as the store underneath
/// does, and that none of them is recorded.
#[test]
fn everything_else_is_forwarded_and_not_recorded() {
    let inner = fake();
    let recording = RecordingStore::new(Arc::clone(&inner));
    recording
        .txn(
            Txn::new()
                .put("p/a", "1", Cond::Any)
                .put("p/b", "2", Cond::Any),
        )
        .expect("store")
        .expect("commit");
    recording.clear();
    assert_eq!(
        recording.get("p/a").expect("get"),
        inner.get("p/a").expect("get")
    );
    assert_eq!(recording.scan_prefix("p/").expect("scan").len(), 2);
    let page = recording
        .scan_range(ScanRange::new("p/", std::num::NonZeroUsize::MIN))
        .expect("range");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        recording
            .changes(Revision::ZERO, 10)
            .expect("feed")
            .rows
            .len(),
        2
    );
    assert_eq!(recording.commit_stats(), inner.commit_stats());
    assert_eq!(recording.now(), inner.now());
    // A trim is forwarded, deletes through the store underneath, and is not a write.
    assert_eq!(recording.trim_feed(Revision(1)), Ok(1));
    assert_eq!(
        inner.changes(Revision::ZERO, 10),
        Err(StoreError::StaleCursor {
            after: Revision::ZERO,
            floor: Revision(1)
        })
    );
    assert_eq!(recording.writes(), Vec::new());
}

// A decision committed through the recording store is recorded as a committed write of each key it
// writes, one that conflicts as a write that did not commit, and one that declines is not
// recorded; the head is the inner store's. It catches a claim that bypasses the record.
#[test]
fn a_decided_transaction_is_recorded_as_it_committed_or_conflicted() {
    use js_state::Claim;
    let recording = RecordingStore::new(fake());
    recording
        .txn(Txn::new().put("q", "queued", Cond::Any))
        .unwrap()
        .unwrap();
    let claim = ["q".to_owned()];
    let placed = recording
        .claim_decide(&claim, &mut |_| {
            Some(Txn::new().put("p", "1", Cond::Absent))
        })
        .unwrap();
    assert!(matches!(placed, Claim::Committed(_)), "{placed:?}");
    let conflicted = recording
        .claim_decide(&claim, &mut |_| {
            Some(Txn::new().put("p", "2", Cond::Absent))
        })
        .unwrap();
    assert!(matches!(conflicted, Claim::Conflict(_)), "{conflicted:?}");
    let declined = recording.claim_decide(&claim, &mut |_| None).unwrap();
    assert_eq!(declined, Claim::Declined);
    assert_eq!(
        recording.writes(),
        [
            write("q", WriteKind::Put, true),
            write("p", WriteKind::Put, true),
            write("p", WriteKind::Put, false),
        ]
    );
    assert!(recording.head().unwrap().is_some());
}

/// Asserts that every get, batched get, prefix scan, range scan and clock read is recorded in the order it
/// was asked for, with its key or prefix, that the feed, the head, the counters and a write
/// are not reads, and that `clear` forgets the reads too. It catches a recorder that misses a
/// kind of read (a test that counts a component's round trips would then count too few) and
/// one that records a read as another kind.
#[test]
fn every_read_is_recorded_in_order_with_its_key_or_prefix() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(Txn::new().put("r/a", "1", Cond::Any))
        .expect("store")
        .expect("commit");
    assert_eq!(recording.reads(), Vec::new());
    recording.get("r/a").expect("get");
    recording.get("r/none").expect("get");
    let many = recording
        .get_many(&["r/a".to_owned(), "r/none".to_owned()])
        .expect("get many");
    assert_eq!(many.len(), 1);
    recording.scan_prefix("r/").expect("scan");
    recording
        .scan_range(ScanRange::new("s/", std::num::NonZeroUsize::MIN))
        .expect("range");
    recording.now().expect("the clock");
    recording.changes(Revision::ZERO, 10).expect("feed");
    recording.head().expect("head");
    let _ = recording.commit_stats();
    assert_eq!(
        recording.reads(),
        [
            Read::Get("r/a".to_owned()),
            Read::Get("r/none".to_owned()),
            Read::GetMany(vec!["r/a".to_owned(), "r/none".to_owned()]),
            Read::ScanPrefix("r/".to_owned()),
            Read::ScanRange("s/".to_owned()),
            Read::Now,
        ]
    );
    recording.clear();
    assert_eq!(recording.reads(), Vec::new());
}

/// Asserts that a stamped transaction is recorded as a write of each key it puts, committed or
/// not as it was, that one whose build declines is not recorded, and that its clock reading is
/// the store's own, read inside the call and not recorded as a read. It catches a stamped
/// commit that bypasses the record, and a recorder that counts its clock as a read of the
/// caller's.
#[test]
fn a_stamped_transaction_is_recorded_as_it_committed_and_its_clock_is_not_a_read() {
    let recording = RecordingStore::new(fake());
    let mut stamps = Vec::new();
    let mut put = |value: &'static str| {
        recording
            .txn_stamped(&mut |now| {
                stamps.push(now);
                Some(Txn::new().put("t", value, Cond::Absent))
            })
            .expect("store")
    };
    assert!(matches!(put("1"), Some(Ok(_))));
    assert!(matches!(put("2"), Some(Err(_))));
    let declined = recording.txn_stamped(&mut |_| None).expect("store");
    assert_eq!(declined, None);
    // The fake's clock starts at zero; its commit moves it on.
    assert_eq!(stamps.len(), 2);
    assert_eq!(stamps[0], StoreTime::ZERO);
    assert_eq!(
        recording.writes(),
        [
            write("t", WriteKind::Put, true),
            write("t", WriteKind::Put, false)
        ]
    );
    assert_eq!(recording.reads(), Vec::new());
}
