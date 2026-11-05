//! The ended-allocation rows: the id, the names, the stored form, and the key space over the
//! in-memory fake and over a real SQLite file.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Clock, Cond, ManualClock, Store, Txn};
use js_testkit::dir::TestDir;

use super::*;
use crate::testkit::TestStore;

/// A row with every field present and every figure different, ending at `end`.
pub(crate) fn row(end: u64) -> EndedRow {
    EndedRow {
        id: AllocationId::parse("7-123").unwrap(),
        profile: Some(ProfileKey::parse("build.fast").unwrap()),
        host: Some(HostId::new("h1").unwrap()),
        booked: Some(Booked {
            whole_host: true,
            cpu_milli: 4000,
            memory_bytes: 8 << 30,
        }),
        queued_ms: Some(40_000),
        run_ms: Some(150_000),
        ended_reason: EndedReason::StoppedAtWallLimit,
        drain_outcome: DrainOutcome::MarkDropped,
        give_up_outcome: Some(1),
        estimated_end: Some(end.saturating_sub(30_000)),
        actual_end: end,
        error_ms: Some(30_000),
        used: Some(Used {
            cpu_milli: 1500,
            memory_bytes: 1 << 30,
            cpu_mean_milli: 1100,
        }),
    }
}

/// A row with every optional field absent, ending at `end`, named `id`.
fn bare(end: u64, id: &str) -> EndedRow {
    EndedRow {
        id: AllocationId::parse(id).unwrap(),
        profile: None,
        host: None,
        booked: None,
        queued_ms: None,
        run_ms: None,
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

/// The row whose bytes [`the_bytes_of_a_row_are_pinned`] pins: a request cancelled while
/// it waited, at 300 ms.
fn pinned() -> EndedRow {
    EndedRow {
        queued_ms: Some(0),
        run_ms: Some(0),
        ended_reason: EndedReason::CancelledWhileWaiting,
        drain_outcome: DrainOutcome::NoMark,
        give_up_outcome: Some(0),
        ..bare(300, "1-2")
    }
}

fn pinned_bytes() -> Vec<u8> {
    let mut b = vec![1, 3, b'1', b'-', b'2', 0, 0, 0, 1, 0, 1, 0, 23];
    b.extend_from_slice(b"cancelled-while-waiting");
    b.push(7);
    b.extend_from_slice(b"no-mark");
    b.extend_from_slice(&[1, 0, 0, 0xac, 0x02, 0, 0]);
    b
}

/// `bytes` with `at` replaced by `with`.
fn splice(bytes: &[u8], at: std::ops::Range<usize>, with: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.splice(at, with.iter().copied());
    out
}

/// Runs `test` over the in-memory fake and then over a real SQLite file in a scratch
/// directory, each fresh and empty.
fn on_both(name: &str, test: impl Fn(&dyn Store)) {
    let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    test(&fake);
    let dir = TestDir::under(std::env::temp_dir(), &format!("js-usage-{name}"));
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
    let sqlite = SqliteStore::open(&dir.path().join("state.db"), clock, Options::default())
        .expect("open the scratch store");
    test(&sqlite);
}

/// Writes `rows` in one batch, as raw rows under their keys.
fn put_all(store: &dyn Store, rows: &[EndedRow]) {
    let txns = rows.iter().map(|row| {
        let key = ended_key(row.actual_end, &row.id);
        Txn::new().put(key, encode(row), Cond::Any)
    });
    for result in store.apply_batch(txns.collect()).unwrap() {
        result.unwrap();
    }
}

fn ends(read: &EndedRead) -> Vec<u64> {
    read.rows.iter().map(|row| row.actual_end).collect()
}

fn all(store: &dyn Store) -> EndedRead {
    let limit = NonZeroUsize::new(100_000).unwrap();
    EndedHistory::new(store).range(0, u64::MAX, limit).unwrap()
}

/// Catches: an id that could break a key (a `/`, an empty or overlong one) or that a job id
/// could not be, accepted; and the boundary lengths refused.
#[test]
fn an_allocation_id_is_one_to_63_bytes_of_the_job_id_alphabet() {
    assert_eq!(AllocationId::parse("a.b_c-9").unwrap().as_str(), "a.b_c-9");
    assert!(AllocationId::parse(&"x".repeat(MAX_ALLOCATION_ID_LEN)).is_ok());
    for bad in ["", "a/b", "A", "a b", "é"] {
        assert_eq!(AllocationId::parse(bad), Err(BadAllocationId), "{bad:?}");
    }
    let long = "x".repeat(MAX_ALLOCATION_ID_LEN + 1);
    assert_eq!(AllocationId::parse(&long), Err(BadAllocationId));
    assert_eq!(
        BadAllocationId.to_string(),
        "an allocation id is 1 to 63 bytes of [a-z0-9._-]"
    );
}

/// Catches: a lease's allocation named by anything but its id's text, and the largest lease
/// id not fitting the id.
#[test]
fn a_lease_names_its_allocation_by_its_decimal_text() {
    let id = AllocationId::of_lease(&LeaseId::new(3, 17));
    assert_eq!(id.to_string(), "3-17");
    let largest = AllocationId::of_lease(&LeaseId::new(u64::MAX, u64::MAX));
    assert_eq!(AllocationId::parse(largest.as_str()), Ok(largest));
}

/// Catches: two reasons or two outcomes sharing a stored name, a name that does not read
/// back as its own value, and a name this build does not know read as some value.
#[test]
fn every_reason_and_drain_outcome_has_its_own_stable_name() {
    let names: Vec<&str> = REASONS.iter().map(|r| r.name()).collect();
    assert_eq!(
        names,
        [
            "released",
            "expired",
            "lost",
            "cancelled-while-waiting",
            "failed-to-start",
            "stopped-at-wall-limit"
        ]
    );
    for reason in REASONS {
        assert_eq!(EndedReason::from_name(reason.name()), Some(reason));
    }
    let names: Vec<&str> = DRAINS.iter().map(|d| d.name()).collect();
    assert_eq!(
        names,
        ["none", "no-mark", "mark-held", "mark-dropped", "unknown"]
    );
    for drain in DRAINS {
        assert_eq!(DrainOutcome::from_name(drain.name()), Some(drain));
    }
    assert_eq!(EndedReason::from_name("evicted"), None);
    assert_eq!(DrainOutcome::from_name("Released"), None);
}

/// Catches: a field lost, swapped or misread on the way through, with every optional field
/// present and with every one absent, and the extremes of the signed error.
#[test]
fn a_row_round_trips_with_every_field_present_and_absent() {
    let full = row(1_000_000);
    assert_eq!(decode(&encode(&full)), Ok(full.clone()));
    let shared = EndedRow {
        booked: Some(Booked {
            whole_host: false,
            cpu_milli: 1,
            memory_bytes: 2,
        }),
        ..full.clone()
    };
    assert_eq!(decode(&encode(&shared)), Ok(shared));
    let bare = bare(5, "0");
    assert_eq!(decode(&encode(&bare)), Ok(bare));
    for error in [i64::MIN, -1, 0, 1, i64::MAX] {
        let signed = EndedRow {
            error_ms: Some(error),
            ..full.clone()
        };
        assert_eq!(decode(&encode(&signed)), Ok(signed));
    }
}

/// Catches: a changed byte layout (a stored row must stay readable across releases).
#[test]
fn the_bytes_of_a_row_are_pinned() {
    assert_eq!(encode(&pinned()), pinned_bytes());
    assert_eq!(decode(&pinned_bytes()), Ok(pinned()));
}

/// Catches: a reader that refuses a row a later build wrote with a field added at the end,
/// which would make every added field cost the older build its history.
#[test]
fn bytes_after_the_known_fields_are_a_later_builds_fields_and_are_ignored() {
    let mut later = encode(&row(9));
    later.extend_from_slice(&[1, 0x81, 0x01, 7]);
    assert_eq!(decode(&later), Ok(row(9)));
}

/// Catches: a row of another version read as this one.
#[test]
fn a_row_of_another_version_is_refused() {
    let mut newer = encode(&row(9));
    newer[0] = 2;
    assert_eq!(decode(&newer), Err(EndedRowError::Version(2)));
    assert_eq!(decode(&[0]), Err(EndedRowError::Version(0)));
    assert_eq!(
        EndedRowError::Version(2).to_string(),
        "ended-allocation row version 2 is not read by this build"
    );
}

/// Catches: a short value decoding to a partial row, at every cut point.
#[test]
fn a_truncated_row_is_refused_at_every_length() {
    for bytes in [encode(&row(1 << 40)), pinned_bytes()] {
        for cut in 0..bytes.len() {
            assert_eq!(
                decode(&bytes[..cut]),
                Err(EndedRowError::Truncated),
                "cut at {cut}"
            );
        }
    }
}

/// Catches: a value a field cannot hold read as some value: an id, a profile key or a host
/// that is not one, text that is not UTF-8, a presence or a whole-host byte other than 0
/// or 1, and a reason or an outcome this build does not know. The offsets are those of
/// [`pinned_bytes`]: the id at 2, the presences of the profile, the host and the booking at
/// 5, 6 and 7, the reason's text at 13 and the outcome's at 37.
#[test]
fn a_field_this_build_does_not_read_is_refused_by_name() {
    let b = pinned_bytes();
    let field = EndedRowError::Field;
    let cases: [(Vec<u8>, EndedRowError); 9] = [
        (splice(&b, 2..3, b"A"), field("id")),
        (splice(&b, 2..3, &[0xff]), field("id")),
        (splice(&b, 5..6, &[2]), field("profile")),
        (splice(&b, 5..6, &[1, 1, b'A']), field("profile")),
        (splice(&b, 6..7, &[1, 1, b'A']), field("host")),
        (splice(&b, 7..8, &[1, 2, 0, 0]), field("booked")),
        (splice(&b, 13..14, b"x"), field("ended_reason")),
        (splice(&b, 37..38, b"x"), field("drain_outcome")),
        (splice(&b, 37..38, &[0xff]), field("drain_outcome")),
    ];
    for (bytes, expected) in cases {
        assert_eq!(decode(&bytes), Err(expected));
    }
    assert_eq!(
        field("host").to_string(),
        "ended-allocation row holds a bad host"
    );
}

/// Catches: a varint of eleven bytes, or with bits past the 64th, wrapping silently, and a
/// give-up count too large for its field cut to fit. The actual end is at 47 and 48 of
/// [`pinned_bytes`], the give-up count's presence at 44.
#[test]
fn numbers_out_of_range_are_refused() {
    let b = pinned_bytes();
    let top = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
    let largest = splice(&b, 47..49, &top);
    assert_eq!(decode(&largest).unwrap().actual_end, u64::MAX);
    let wraps = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
    let endless = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x81];
    let range = Err(EndedRowError::Range);
    assert_eq!(decode(&splice(&b, 47..49, &wraps)), range);
    assert_eq!(decode(&splice(&b, 47..49, &endless)), range);
    let past_u32 = [1, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert_eq!(decode(&splice(&b, 44..46, &past_u32)), range);
}

/// Catches: a row written twice stored twice or replaced (the key is the end and the id, and
/// the write is conditional on the key being absent), and a row stored under a key a range
/// scan would not find in time order.
#[test]
fn a_row_is_written_once_under_its_end_and_id() {
    on_both("ended-once", |store| {
        let history = EndedHistory::new(store);
        let first = row(20_000);
        assert_eq!(history.put(&first), Ok(true));
        let again = EndedRow {
            used: None,
            ..first.clone()
        };
        assert_eq!(history.put(&again), Ok(false));
        let stored = store.scan_prefix(ENDED_PREFIX).unwrap();
        let keys: Vec<&str> = stored.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["usage/ended/00000000000000020000/7-123"]);
        assert_eq!(all(store).rows, [first]);
    });
}

/// Asserts that the two writes a writer puts in its own transaction are the index entry,
/// naming the row's key, and the row under that key, and that a range reads the row back
/// once they are committed. It catches writes under another key than `put` uses, or a row
/// stored in a form the range cannot read.
#[test]
fn the_writes_of_a_row_are_its_index_entry_and_the_row_under_its_key() {
    let first = row(20_000);
    let [index, stored] = ended_writes(&first);
    let key = "usage/ended/00000000000000020000/7-123";
    assert_eq!(
        index,
        ("usage/ended-id/7-123".to_owned(), key.as_bytes().to_vec())
    );
    assert_eq!(stored, (key.to_owned(), encode(&first)));
    let store = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    let txn = Txn::new()
        .put(index.0, index.1, Cond::Any)
        .put(stored.0, stored.1, Cond::Any);
    store.txn(txn).unwrap().unwrap();
    assert_eq!(all(&store).rows, [first]);
}

/// Catches: a write error swallowed as written.
#[test]
fn a_failed_write_writes_nothing_and_says_so() {
    let store = TestStore::new();
    TestStore::set(&store.fail_txn, true);
    let history = EndedHistory::new(&*store);
    assert!(matches!(history.put(&row(5)), Err(UsageError::Store(_))));
    assert!(store.fake.scan_prefix(ENDED_PREFIX).unwrap().is_empty());
}

/// Catches: rows read out of time order, an end at either bound left out, an end past a
/// bound let in, the limit not kept, a read that stops at the first page, and rows of one
/// end out of id order. 300 rows span more than one page.
#[test]
fn rows_read_back_in_time_order_within_inclusive_bounds_and_the_limit() {
    on_both("ended-range", |store| {
        let mut rows: Vec<EndedRow> = (0..300).map(|i| bare(1_000 + i * 10, "a")).collect();
        rows.push(bare(1_010, "b"));
        rows.reverse();
        put_all(store, &rows);
        let history = EndedHistory::new(store);
        let many = NonZeroUsize::new(1_000).unwrap();
        let read = history.range(1_010, 3_990, many).unwrap();
        assert_eq!(read.skipped, 0);
        assert_eq!(read.rows.len(), 300);
        assert_eq!(ends(&read)[..3], [1_010, 1_010, 1_020]);
        let ids: Vec<&str> = read.rows[..2].iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(ends(&read).last(), Some(&3_990));
        assert!(ends(&read).is_sorted());
        let few = NonZeroUsize::new(3).unwrap();
        assert_eq!(
            ends(&history.range(0, u64::MAX, few).unwrap()),
            [1_000, 1_010, 1_010]
        );
        assert!(history.range(1_011, 1_019, many).unwrap().rows.is_empty());
        assert!(history.range(2_000, 1_000, many).unwrap().rows.is_empty());
        assert_eq!(all(store).rows.len(), 301);
    });
}

/// Catches: a row of a later version, or bytes that do not decode, failing the whole read
/// (one bad row would hide the history around it) or being counted as a row.
#[test]
fn a_row_this_build_does_not_read_is_skipped_not_fatal() {
    on_both("ended-skip", |store| {
        put_all(store, &[row(10), row(30)]);
        let mut newer = encode(&row(20));
        newer[0] = 2;
        let id = AllocationId::parse("9-9").unwrap();
        let txn = Txn::new().put(ended_key(20, &id), newer, Cond::Absent).put(
            ended_key(25, &id),
            vec![1],
            Cond::Absent,
        );
        store.txn(txn).unwrap().unwrap();
        let read = all(store);
        assert_eq!(ends(&read), [10, 30]);
        assert_eq!(read.skipped, 2);
    });
}

/// Catches: `<=` for `<` at the window (the row that ended exactly at the window lost a
/// millisecond early), a row inside the window deleted, a prune that stops at the first page
/// (300 old rows), and a prune with nothing to delete failing or deleting.
#[test]
fn history_older_than_the_retention_is_pruned_and_newer_kept() {
    on_both("ended-prune", |store| {
        const DAY: u64 = 86_400_000;
        let now = 100 * DAY;
        let window = 90 * DAY;
        let history = EndedHistory::new(store);
        let mut rows: Vec<EndedRow> = (0..300).map(|i| bare(i, "old")).collect();
        rows.push(bare(now - window - 1, "edge-1"));
        rows.push(bare(now - window, "edge"));
        rows.push(bare(now - 1, "recent"));
        put_all(store, &rows);
        assert_eq!(history.prune(now, window), Ok(301));
        let kept: Vec<String> = all(store).rows.iter().map(|r| r.id.to_string()).collect();
        assert_eq!(kept, ["edge", "recent"]);
        assert_eq!(history.prune(now, window), Ok(0));
        assert_eq!(history.prune(now + 1, window), Ok(1));
        // A window longer than the clock has run keeps everything.
        assert_eq!(history.prune(now, u64::MAX), Ok(0));
        assert_eq!(all(store).rows.len(), 1);
    });
}

/// Catches: a log that keeps rows when it was meant to keep none (a no-op that panics or
/// blocks would cost the caller its call).
#[test]
fn the_empty_log_takes_a_row_and_keeps_nothing() {
    let log: &dyn AllocationLog = &NoLog;
    log.ended(row(1));
}

/// Writes raw `value` under the row key of `end` and `id`, unconditionally.
fn put_raw(store: &dyn Store, key: String, value: Vec<u8>) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .unwrap()
        .unwrap();
}

/// Catches: an allocation recorded a second time under a later end (a lease put back after
/// a restart and stopped again), an index entry that does not name the row, and a prune
/// that leaves the index entry of a row it deleted.
#[test]
fn an_allocation_is_recorded_once_whatever_end_a_later_write_gives_it() {
    on_both("ended-index", |store| {
        let history = EndedHistory::new(store);
        let first = row(20_000);
        assert_eq!(history.put(&first), Ok(true));
        assert_eq!(history.put(&row(30_000)), Ok(false));
        assert_eq!(all(store).rows, std::slice::from_ref(&first));
        let index = store.get(&ended_id_key(&first.id)).unwrap().unwrap();
        assert_eq!(index.value, ended_key(20_000, &first.id).into_bytes());
        assert_eq!(history.prune(20_001, 0), Ok(1));
        assert!(store.scan_prefix(ENDED_PREFIX).unwrap().is_empty());
        assert!(store.scan_prefix(ENDED_ID_PREFIX).unwrap().is_empty());
    });
}

/// Catches: a row returned under a key its own end and id do not give (it would be read
/// in another range, and out of order), and such a row counted as read.
#[test]
fn a_row_under_a_key_its_end_and_id_do_not_give_is_skipped() {
    on_both("ended-mismatch", |store| {
        let moved = format!("{ENDED_PREFIX}{:020}/7-123", 20);
        put_raw(store, moved, encode(&row(10)));
        let renamed = ended_key(10, &AllocationId::parse("9-9").unwrap());
        put_raw(store, renamed, encode(&row(10)));
        let read = all(store);
        assert_eq!((read.rows.len(), read.skipped), (0, 2));
        let early = EndedHistory::new(store)
            .range(0, 15, NonZeroUsize::MIN)
            .unwrap();
        assert_eq!((early.rows.len(), early.skipped), (0, 1));
    });
}

/// Catches: a read that stops at a page holding no readable row (300 unreadable rows fill
/// more than a page) and so misses the rows after it, unreadable rows using up a small
/// limit, and a range of unreadable rows only read as an error.
#[test]
fn an_unreadable_page_is_read_past_to_the_rows_after_it() {
    on_both("ended-unreadable", |store| {
        let id = AllocationId::parse("x").unwrap();
        for end in 1..=300 {
            put_raw(store, ended_key(end, &id), vec![9]);
        }
        put_all(store, &[row(1_000)]);
        let history = EndedHistory::new(store);
        let first = history.range(0, u64::MAX, NonZeroUsize::MIN).unwrap();
        assert_eq!((ends(&first), first.skipped), (vec![1_000], 300));
        let none = history.range(0, 500, NonZeroUsize::MIN).unwrap();
        assert_eq!((none.rows.len(), none.skipped), (0, 300));
    });
}

/// Catches: a key of the first or the last millisecond a row can name that a range or a
/// prune bound leaves out or lets in wrongly.
#[test]
fn rows_at_the_ends_of_time_are_read_and_pruned() {
    on_both("ended-extremes", |store| {
        put_all(store, &[bare(0, "first"), bare(u64::MAX, "last")]);
        let history = EndedHistory::new(store);
        let one = NonZeroUsize::new(10).unwrap();
        assert_eq!(ends(&history.range(0, 0, one).unwrap()), [0]);
        assert_eq!(
            ends(&history.range(u64::MAX, u64::MAX, one).unwrap()),
            [u64::MAX]
        );
        assert_eq!(ends(&all(store)), [0, u64::MAX]);
        assert_eq!(history.prune(u64::MAX, 0), Ok(1));
        assert_eq!(ends(&all(store)), [u64::MAX]);
    });
}

/// A fake store that fails the `n`th scan or batch on cue, and can rewrite a key right
/// after a scan (as a second writer would between a prune's scan and its delete).
struct Hooked {
    inner: Fake,
    scans: std::sync::atomic::AtomicUsize,
    batches: std::sync::atomic::AtomicUsize,
    fail_scan: std::sync::atomic::AtomicUsize,
    fail_batch: std::sync::atomic::AtomicUsize,
    after_scan: std::sync::Mutex<Option<(String, Vec<u8>)>>,
}

impl Hooked {
    fn new() -> Self {
        let zero = || std::sync::atomic::AtomicUsize::new(0);
        Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            scans: zero(),
            batches: zero(),
            fail_scan: zero(),
            fail_batch: zero(),
            after_scan: std::sync::Mutex::new(None),
        }
    }

    /// Fails the `n`th scan from now on (counting from 1), and no other.
    fn fail_scan(&self, n: usize) {
        use std::sync::atomic::Ordering::SeqCst;
        self.fail_scan.store(self.scans.load(SeqCst) + n, SeqCst);
    }

    /// Fails the `n`th batch from now on (counting from 1), and no other.
    fn fail_batch(&self, n: usize) {
        use std::sync::atomic::Ordering::SeqCst;
        self.fail_batch.store(self.batches.load(SeqCst) + n, SeqCst);
    }
}

/// Counts one call on `counter` and says whether it is the one to fail.
fn hit(counter: &std::sync::atomic::AtomicUsize, fail_at: &std::sync::atomic::AtomicUsize) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    counter.fetch_add(1, SeqCst) + 1 == fail_at.load(SeqCst)
}

impl Store for Hooked {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<js_state::Commit, js_state::Conflict>>, js_state::StoreError> {
        match hit(&self.batches, &self.fail_batch) {
            true => Err(js_state::StoreError::Failed("down".to_owned())),
            false => self.inner.apply_batch(batch),
        }
    }

    fn get(&self, key: &str) -> Result<Option<js_state::Versioned>, js_state::StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, js_state::Versioned)>, js_state::StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<js_state::ScanPage, js_state::StoreError> {
        if hit(&self.scans, &self.fail_scan) {
            return Err(js_state::StoreError::Failed("down".to_owned()));
        }
        let page = self.inner.scan_range(scan)?;
        let rewrite = self.after_scan.lock().unwrap().take();
        rewrite
            .into_iter()
            .for_each(|(key, value)| put_raw(&self.inner, key, value));
        Ok(page)
    }

    fn changes(
        &self,
        after: js_state::Revision,
        limit: usize,
    ) -> Result<js_state::Changes, js_state::StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, js_state::StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> js_state::CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> js_state::StoreTime {
        self.inner.now()
    }
}

/// `n` rows ending at 1 to `n`, each its own allocation.
fn many(n: u64) -> Vec<EndedRow> {
    (1..=n).map(|end| bare(end, &format!("r{end}"))).collect()
}

/// Catches: a failed scan, on the first page or on a later one, read as fewer rows rather
/// than reported.
#[test]
fn a_scan_that_fails_on_any_page_fails_the_read() {
    let store = Hooked::new();
    put_all(&store, &many(300));
    let history = EndedHistory::new(&store);
    let limit = NonZeroUsize::new(1_000).unwrap();
    for page in [1, 2] {
        store.fail_scan(page);
        let failed = history.range(0, u64::MAX, limit);
        assert!(matches!(failed, Err(UsageError::Store(_))), "page {page}");
    }
    assert_eq!(history.range(0, u64::MAX, limit).unwrap().rows.len(), 300);
}

/// Catches: a prune whose delete fails after an earlier page's deletes reporting success,
/// losing the deletes already made, or not finishing on its next run; and a scan failure
/// of a prune swallowed.
#[test]
fn a_prune_that_fails_part_way_keeps_what_it_deleted_and_finishes_on_a_retry() {
    let store = Hooked::new();
    put_all(&store, &many(300));
    let history = EndedHistory::new(&store);
    store.fail_batch(2);
    assert!(matches!(
        history.prune(10_000, 0),
        Err(UsageError::Store(_))
    ));
    assert_eq!(all(&store).rows.len(), 44);
    store.fail_scan(1);
    assert!(matches!(
        history.prune(10_000, 0),
        Err(UsageError::Store(_))
    ));
    assert_eq!(history.prune(10_000, 0), Ok(44));
    assert!(all(&store).rows.is_empty());
}

/// Catches: a row rewritten by another writer between the prune's scan and its delete
/// deleted anyway (the delete is conditional on the version the scan saw), and its index
/// entry deleted without it.
#[test]
fn a_row_rewritten_between_the_scan_and_its_delete_is_kept() {
    let store = Hooked::new();
    let history = EndedHistory::new(&store);
    let rows = many(3);
    for row in &rows {
        assert_eq!(history.put(row), Ok(true));
    }
    let key = ended_key(2, &rows[1].id);
    *store.after_scan.lock().unwrap() = Some((key.clone(), encode(&rows[1])));
    assert_eq!(history.prune(10_000, 0), Ok(2));
    assert_eq!(ends(&all(&store)), [2]);
    let index = store.get(&ended_id_key(&rows[1].id)).unwrap();
    assert_eq!(index.map(|held| held.value), Some(key.into_bytes()));
}

/// A row named `id`, ending at `end`, booked on `host` when one is given.
fn on_host(end: u64, id: &str, host: Option<&str>) -> EndedRow {
    EndedRow {
        host: host.map(|name| HostId::new(name).unwrap()),
        ..bare(end, id)
    }
}

/// The query of every row, `limit` to a page, after `after`.
fn query(after: Option<EndedKey>, limit: usize) -> EndedQuery {
    EndedQuery {
        after,
        from_ms: 0,
        before_ms: None,
        host: None,
        id: None,
        limit: NonZeroUsize::new(limit).unwrap(),
    }
}

/// The `(end, id)` of each row of `page`.
fn named(page: &EndedPage) -> Vec<(u64, &str)> {
    let rows = page.rows.iter();
    rows.map(|row| (row.actual_end, row.id.as_str())).collect()
}

/// The key of the row named `id` that ended at `end`.
fn key_of(end: u64, id: &str) -> EndedKey {
    EndedKey {
        ended_at_ms: end,
        id: AllocationId::parse(id).unwrap(),
    }
}

/// Asserts that each page starts strictly after the cursor the page before it gave, also
/// when the cursor is one of two rows of the same end, that the last page gives no cursor,
/// and that a row written between two pages behind the cursor is not read while one ahead
/// of it is. It catches a cursor read as included (a row twice), a cursor that skips the
/// other rows of its end, and a last page that still points to a next.
#[test]
fn ended_rows_after_resumes_exactly_after_the_cursor() {
    on_both("ended-page-cursor", |store| {
        let rows = [
            bare(10, "a"),
            bare(20, "a"),
            bare(20, "b"),
            bare(30, "a"),
            bare(40, "a"),
        ];
        put_all(store, &rows);
        let history = EndedHistory::new(store);
        let first = history.page(&query(None, 2)).unwrap();
        assert_eq!(named(&first), [(10, "a"), (20, "a")]);
        assert_eq!(first.next, Some(key_of(20, "a")));
        put_all(store, &[bare(15, "z"), bare(35, "z")]);
        let second = history.page(&query(first.next, 2)).unwrap();
        assert_eq!(named(&second), [(20, "b"), (30, "a")]);
        assert_eq!(second.next, Some(key_of(30, "a")));
        let last = history.page(&query(second.next, 2)).unwrap();
        assert_eq!(named(&last), [(35, "z"), (40, "a")]);
        assert_eq!(last.next, None);
        let after_all = history.page(&query(Some(key_of(40, "a")), 2)).unwrap();
        assert_eq!(after_all, EndedPage::default());
    });
}

/// Asserts that a page holds the rows that ended at or after `from_ms` and before
/// `before_ms`, that a cursor before `from_ms` reads from `from_ms`, and that with no
/// `before_ms` the row of the last millisecond is read. It catches an included upper end,
/// an excluded lower one, and a cursor that drags the window back.
#[test]
fn a_page_is_the_half_open_window_of_ends() {
    on_both("ended-page-window", |store| {
        put_all(
            store,
            &[
                bare(19, "a"),
                bare(20, "a"),
                bare(39, "a"),
                bare(40, "a"),
                bare(u64::MAX, "a"),
            ],
        );
        let history = EndedHistory::new(store);
        let window = EndedQuery {
            from_ms: 20,
            before_ms: Some(40),
            ..query(None, 10)
        };
        let page = history.page(&window).unwrap();
        assert_eq!(named(&page), [(20, "a"), (39, "a")]);
        let behind = EndedQuery {
            after: Some(key_of(5, "a")),
            ..window.clone()
        };
        assert_eq!(history.page(&behind).unwrap(), page);
        let open = EndedQuery {
            from_ms: 40,
            ..query(None, 10)
        };
        let page = history.page(&open).unwrap();
        assert_eq!(named(&page), [(40, "a"), (u64::MAX, "a")]);
        let empty = EndedQuery {
            before_ms: Some(20),
            ..window
        };
        assert_eq!(history.page(&empty).unwrap(), EndedPage::default());
    });
}

/// Asserts that the host and allocation filters are applied before the limit counts: of
/// seven rows, the two of one host fill a page of two with no next page, the rows with no
/// host are left out of a host's listing, and an allocation's filter returns its one row.
/// It catches a filter applied to a page after the limit cut it (a short or empty page
/// with a next cursor).
#[test]
fn the_filters_apply_before_the_limit() {
    on_both("ended-page-filters", |store| {
        let rows = [
            on_host(1, "a", Some("h1")),
            on_host(2, "b", None),
            on_host(3, "c", Some("h1")),
            on_host(4, "d", Some("h2")),
            on_host(5, "e", Some("h1")),
            on_host(6, "f", None),
            on_host(7, "g", Some("h2")),
        ];
        put_all(store, &rows);
        let history = EndedHistory::new(store);
        let h2 = EndedQuery {
            host: Some(HostId::new("h2").unwrap()),
            ..query(None, 2)
        };
        let page = history.page(&h2).unwrap();
        assert_eq!(named(&page), [(4, "d"), (7, "g")]);
        assert_eq!(page.next, None);
        let h1 = EndedQuery {
            host: Some(HostId::new("h1").unwrap()),
            ..query(None, 2)
        };
        let page = history.page(&h1).unwrap();
        assert_eq!(named(&page), [(1, "a"), (3, "c")]);
        assert_eq!(page.next, Some(key_of(3, "c")));
        let one = EndedQuery {
            id: Some(AllocationId::parse("f").unwrap()),
            ..query(None, 2)
        };
        let page = history.page(&one).unwrap();
        assert_eq!(named(&page), [(6, "f")]);
        assert_eq!(page.next, None);
        let both = EndedQuery {
            host: Some(HostId::new("h1").unwrap()),
            ..one
        };
        assert_eq!(history.page(&both).unwrap(), EndedPage::default());
    });
}

/// Asserts that a page reads on past a scan page of rows (more than 256), and stops at the
/// first scan page when that already fills it, that rows this build does not read, or under
/// a key their own end and id do not give, are left out and do not use up the limit, and
/// that a page that ends exactly at the last row gives no cursor. It catches a page cut at the first scan page and unreadable rows counted.
#[test]
fn a_page_reads_past_scan_pages_and_unreadable_rows() {
    on_both("ended-page-scan", |store| {
        let id = AllocationId::parse("x").unwrap();
        put_raw(store, ended_key(0, &id), vec![9]);
        put_raw(store, ended_key(500, &id), encode(&bare(600, "x")));
        put_all(store, &many(300));
        let history = EndedHistory::new(store);
        let page = history.page(&query(None, 280)).unwrap();
        assert_eq!(page.rows.len(), 280);
        assert_eq!(page.rows[0].actual_end, 1);
        assert_eq!(page.next, Some(key_of(280, "r280")));
        let rest = history.page(&query(page.next, 280)).unwrap();
        assert_eq!(rest.rows.len(), 20);
        assert_eq!(rest.next, None);
        let whole = history.page(&query(None, 300)).unwrap();
        assert_eq!((whole.rows.len(), whole.next), (300, None));
        let short = history.page(&query(None, 10)).unwrap();
        assert_eq!(short.rows.len(), 10);
        assert_eq!(short.next, Some(key_of(10, "r10")));
        let resumed = history.page(&query(short.next, 1)).unwrap();
        assert_eq!(named(&resumed), [(11, "r11")]);
    });
}

/// Catches: a failed scan, on the first scan page or a later one, read as a shorter page
/// rather than reported.
#[test]
fn a_scan_that_fails_fails_the_page() {
    let store = Hooked::new();
    put_all(&store, &many(300));
    let history = EndedHistory::new(&store);
    for scan in [1, 2] {
        store.fail_scan(scan);
        let failed = history.page(&query(None, 1_000));
        assert!(matches!(failed, Err(UsageError::Store(_))), "scan {scan}");
    }
    assert_eq!(history.page(&query(None, 1_000)).unwrap().rows.len(), 300);
}

/// Asserts that a key's text is the row's store key without its prefix, that it reads
/// back as the same key, and that every text no key has is refused: no `/`, an end of
/// other than 20 digits, a sign or other non-digit, an end past `u64::MAX`, and an id
/// that is not one. It catches a cursor a client can bend (a signed or short end that
/// reads as another key) and a text that panics.
#[test]
fn a_key_reads_back_from_its_text_and_other_text_is_refused() {
    let key = key_of(1_234, "7-123");
    assert_eq!(key.text(), "00000000000000001234/7-123");
    assert_eq!(
        format!("{ENDED_PREFIX}{}", key.text()),
        ended_key(1_234, &key.id)
    );
    assert_eq!(EndedKey::parse(&key.text()), Some(key));
    let last = key_of(u64::MAX, "a");
    assert_eq!(EndedKey::parse(&last.text()), Some(last));
    assert_eq!(EndedKey::of(&bare(5, "b")), key_of(5, "b"));
    for text in [
        "",
        "00000000000000001234",
        "0000000000000001234/a",
        "000000000000000001234/a",
        "+0000000000000001234/a",
        "0000000000000000123x/a",
        "99999999999999999999/a",
        "00000000000000001234/",
        "00000000000000001234/A",
        "00000000000000001234/a/b",
    ] {
        assert_eq!(EndedKey::parse(text), None, "{text:?}");
    }
}
