//! The tail counters over the in-memory fake and over a real SQLite file.
//!
//! Every test runs on both stores and names the defect it would catch. The conflict and
//! failure arms are driven by a store that delegates and fails or lets another writer in
//! on cue (`Hooked`). Nothing sleeps and no test reads a clock.

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_state::sqlite::{Options, SqliteStore};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use js_testkit::dir::TestDir;
use js_usage::{
    NoTail, StoreTailSink, TAIL_BUCKETS, TAIL_PREFIX, TAIL_WRITE_TRIES, TailDelta, TailRowError,
    TailSink, TailSinkCounters, TailSinkError, TailTotals,
};

/// The edges the sinks under test count by (the registry's `ERROR_EDGES_MS`).
const EDGES: [i64; TAIL_BUCKETS - 1] = [-300_000, -60_000, -10_000, 0, 10_000, 60_000, 300_000];

/// The key of the edges row.
const EDGES_KEY: &str = "usage/tail/error_edges_ms";

/// A store that delegates, and can fail a scan or a transaction, or write a row through
/// the store underneath right after a scan, as a second writer that got in first.
struct Hooked {
    inner: Arc<dyn Store>,
    fail_scan: AtomicBool,
    fail_batch: AtomicBool,
    /// One write is made after each scan, in order, while any are left.
    after_scan: Mutex<VecDeque<(String, Vec<u8>)>>,
    /// Transactions asked of the store.
    txns: AtomicUsize,
}

impl Hooked {
    fn over(inner: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_scan: AtomicBool::new(false),
            fail_batch: AtomicBool::new(false),
            after_scan: Mutex::new(VecDeque::new()),
            txns: AtomicUsize::new(0),
        })
    }

    fn after_scan(&self, key: &str, value: Vec<u8>) {
        let mut queue = self.after_scan.lock().expect("lock");
        queue.push_back((key.to_owned(), value));
    }
}

fn down() -> StoreError {
    StoreError::Failed("down".to_owned())
}

impl Store for Hooked {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.txns.fetch_add(1, Ordering::SeqCst);
        if self.fail_batch.load(Ordering::SeqCst) {
            return Err(down());
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if self.fail_scan.load(Ordering::SeqCst) {
            return Err(down());
        }
        let rows = self.inner.scan_prefix(prefix)?;
        let next = self.after_scan.lock().expect("lock").pop_front();
        if let Some((key, value)) = next {
            common::put(&*self.inner, &key, &value);
        }
        Ok(rows)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Runs `test` over the in-memory fake and then over a real SQLite file in a scratch
/// directory, each fresh and empty, behind a [`Hooked`].
fn on_both(name: &str, test: impl Fn(Arc<Hooked>)) {
    test(Hooked::over(Arc::new(common::fake())));
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("js-usage-tail-{name}"),
    );
    let clock = Arc::new(ManualClock::default());
    let sqlite = SqliteStore::open(&dir.path().join("state.db"), clock, Options::default())
        .expect("open the scratch store");
    test(Hooked::over(Arc::new(sqlite)));
}

fn sink(store: &Arc<Hooked>) -> StoreTailSink {
    StoreTailSink::new(store.clone(), EDGES)
}

/// A delta in which every counter differs, the sum negative.
fn every() -> TailDelta {
    TailDelta {
        started_on_marked: 3,
        backfilled: 2,
        ended_estimated: 5,
        ended_unestimated: 7,
        error_sum_ms: -40_000,
        error_buckets: [1, 2, 3, 4, 5, 6, 7, 8],
    }
}

/// The totals of `delta` added to nothing.
fn totals(delta: &TailDelta) -> TailTotals {
    let mut t = TailTotals::default();
    t.add(delta);
    t
}

fn add_and_flush(sink: &StoreTailSink, delta: &TailDelta) {
    sink.add(delta);
    sink.flush().expect("the write lands");
}

/// A counter row of version `version` holding `value`.
fn row(version: u8, value: u64) -> Vec<u8> {
    let mut out = vec![version];
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// The edges row for `edges`.
fn edges_row(edges: &[i64]) -> Vec<u8> {
    let mut out = vec![1, edges.len() as u8];
    edges
        .iter()
        .for_each(|e| out.extend_from_slice(&e.to_le_bytes()));
    out
}

/// Every tail row, as stored.
fn rows(store: &dyn Store) -> Vec<(String, Vec<u8>)> {
    let rows = store.scan_prefix(TAIL_PREFIX).expect("scan");
    rows.into_iter().map(|(k, v)| (k, v.value)).collect()
}

/// Asserts that every counter is written and read back, each in its own row named by
/// its field, with the edges beside them, and that a new sink over the same store (a
/// restart) reads what an earlier one wrote. Catches: a counter that is never written, a
/// swapped field, a sum read back unsigned, and a sink that writes no edges.
#[test]
fn every_counter_is_stored_in_its_own_row_and_read_back() {
    on_both("roundtrip", |store| {
        let first = sink(&store);
        assert_eq!(first.load(), Ok(TailTotals::default()));
        add_and_flush(&first, &every());
        let names: Vec<String> = rows(&*store).into_iter().map(|(k, _)| k).collect();
        let mut expected = vec![
            "usage/tail/backfilled".to_owned(),
            "usage/tail/ended_estimated".to_owned(),
            "usage/tail/ended_unestimated".to_owned(),
        ];
        expected.extend((0..TAIL_BUCKETS).map(|b| format!("usage/tail/error_bucket_{b}")));
        expected.push(EDGES_KEY.to_owned());
        expected.push("usage/tail/error_sum_ms".to_owned());
        expected.push("usage/tail/started_on_marked".to_owned());
        assert_eq!(names, expected);
        let stored: Vec<(String, Vec<u8>)> = rows(&*store);
        let edges = stored.iter().find(|(k, _)| k == EDGES_KEY).expect("edges");
        assert_eq!(edges.1, edges_row(&EDGES));
        let sum = stored.iter().find(|(k, _)| k.ends_with("error_sum_ms"));
        assert_eq!(sum.expect("sum").1, row(1, (-40_000_i64) as u64));
        let restarted = sink(&store);
        assert_eq!(restarted.load(), Ok(totals(&every())));
        assert_eq!(restarted.counters(), TailSinkCounters::default());
    });
}

/// Asserts that two sinks over one store each add their own deltas and the stored total
/// is the sum of all of them, interleaved. Catches: a write that sets a value read at
/// open (or at the sink's last write) instead of adding to the row as it is, which would
/// drop the other writer's counts.
#[test]
fn tail_counter_deltas_from_two_writers_both_land() {
    on_both("two-writers", |store| {
        let (a, b) = (sink(&store), sink(&store));
        let one = TailDelta {
            started_on_marked: 1,
            error_sum_ms: 10,
            ..TailDelta::default()
        };
        let two = TailDelta {
            started_on_marked: 2,
            backfilled: 1,
            error_sum_ms: -25,
            ..TailDelta::default()
        };
        assert_eq!(a.load(), Ok(TailTotals::default()));
        assert_eq!(b.load(), Ok(TailTotals::default()));
        add_and_flush(&a, &one);
        add_and_flush(&b, &two);
        add_and_flush(&a, &two);
        add_and_flush(&b, &one);
        let expected = TailTotals {
            started_on_marked: 6,
            backfilled: 2,
            error_sum_ms: -30,
            ..TailTotals::default()
        };
        assert_eq!(sink(&store).load(), Ok(expected));
    });
}

/// Asserts that when another writer changes a row between this one's read and its
/// conditional write, the write is read again and tried again, and lands once: the
/// other writer's 10 and this one's 1 make 11. Catches: an unconditional write (10
/// lost: 1), a conflict given up on (the delta lost: 10), and a delta applied twice (12).
#[test]
fn a_conflicting_write_is_retried_not_lost() {
    on_both("conflict", |store| {
        let s = sink(&store);
        store.after_scan("usage/tail/started_on_marked", row(1, 10));
        let one = TailDelta {
            started_on_marked: 1,
            ..TailDelta::default()
        };
        add_and_flush(&s, &one);
        assert_eq!(store.txns.load(Ordering::SeqCst), 2);
        let loaded = sink(&store).load().expect("load");
        assert_eq!(loaded.started_on_marked, 11);
        assert_eq!(s.pending(), TailDelta::default());
    });
}

/// Asserts that a write another writer beats on every try gives up with a conflict after
/// [`TAIL_WRITE_TRIES`] tries, keeps its delta for the next flush and counts the failure,
/// and that the next flush lands it once. Catches: a retry loop with no end, and a delta
/// dropped (or doubled) after a write that gave up.
#[test]
fn a_write_beaten_on_every_try_waits_for_the_next_flush() {
    on_both("conflict-always", |store| {
        let s = sink(&store);
        for n in 0..TAIL_WRITE_TRIES {
            store.after_scan("usage/tail/backfilled", row(1, n as u64 + 1));
        }
        let one = TailDelta {
            backfilled: 1,
            ..TailDelta::default()
        };
        s.add(&one);
        let tries = TAIL_WRITE_TRIES;
        assert_eq!(s.flush(), Err(TailSinkError::Conflict { tries }));
        assert_eq!(s.pending(), one);
        assert_eq!(s.counters().writes_failed, 1);
        s.flush().expect("the second flush lands");
        let loaded = sink(&store).load().expect("load");
        assert_eq!(loaded.backfilled, TAIL_WRITE_TRIES as u64 + 1);
    });
}

/// Asserts that a stored set bucketed by other edges is refused with the typed error and
/// zero counters, that a write beside it is refused and dropped, and that the stored rows
/// are byte for byte as they were. Catches: buckets of other ranges read as this build's
/// (a wrong histogram), and a sink that adds to them or rewrites the edges.
#[test]
fn tail_counters_with_other_bucket_edges_are_refused_and_not_misread() {
    on_both("other-edges", |store| {
        let other = [-1, 0, 1, 2, 3, 4, 5];
        common::put(&*store, EDGES_KEY, &edges_row(&other));
        common::put(&*store, "usage/tail/error_bucket_0", &row(1, 9));
        let before = rows(&*store);
        let s = sink(&store);
        let refused = TailSinkError::OtherEdges {
            stored: other.to_vec(),
            build: EDGES.to_vec(),
        };
        assert_eq!(s.load(), Err(refused.clone()));
        s.add(&every());
        assert_eq!(s.flush(), Err(refused));
        assert_eq!(s.pending(), TailDelta::default());
        assert_eq!(s.counters().writes_failed, 1);
        assert_eq!(rows(&*store), before);
    });
}

/// Asserts that an edges row that does not read (empty, cut short before its count,
/// a length that does not match its count, a newer version) refuses the set the same way:
/// a typed error naming the row, and nothing written. Catches: a broken edges row taken
/// as matching edges, and one overwritten by a write.
#[test]
fn an_edges_row_that_does_not_read_refuses_the_set() {
    let cases: [(Vec<u8>, TailRowError); 4] = [
        (Vec::new(), TailRowError::Empty),
        (vec![1], TailRowError::Length(1)),
        (vec![1, 7, 0], TailRowError::Length(3)),
        (vec![2, 0], TailRowError::Version(2)),
    ];
    for (n, (bytes, reason)) in cases.into_iter().enumerate() {
        on_both(&format!("bad-edges-{n}"), |store| {
            common::put(&*store, EDGES_KEY, &bytes);
            let s = sink(&store);
            let unreadable = TailSinkError::Unreadable {
                key: EDGES_KEY.to_owned(),
                reason,
            };
            assert_eq!(s.load(), Err(unreadable.clone()));
            s.add(&every());
            assert_eq!(s.flush(), Err(unreadable));
            assert_eq!(rows(&*store), vec![(EDGES_KEY.to_owned(), bytes.clone())]);
        });
    }
}

/// Asserts that a counter row of a newer version (or one that does not read as this
/// version: empty, or the wrong length) is skipped and counted at a load and at a write,
/// read as zero, never overwritten, while the other counters load and are written as
/// usual; and that a row of a field this build does not know is left alone. Catches: a
/// newer row that fails the whole load, one read as this version's value, one replaced by
/// an older build's write, and a later build's field taken for a counter.
#[test]
fn a_newer_row_version_is_skipped_not_fatal() {
    on_both("newer-row", |store| {
        common::put(&*store, EDGES_KEY, &edges_row(&EDGES));
        let newer = [2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 9];
        common::put(&*store, "usage/tail/backfilled", &newer);
        common::put(&*store, "usage/tail/ended_estimated", &[]);
        common::put(&*store, "usage/tail/ended_unestimated", &[1, 1]);
        common::put(&*store, "usage/tail/started_on_marked", &row(1, 4));
        common::put(&*store, "usage/tail/a_later_field", &[3]);
        let s = sink(&store);
        let loaded = s.load().expect("a newer row is not fatal");
        let expected = TailTotals {
            started_on_marked: 4,
            ..TailTotals::default()
        };
        assert_eq!(loaded, expected);
        assert_eq!(s.counters().rows_skipped, 3);
        let delta = TailDelta {
            started_on_marked: 1,
            backfilled: 1,
            ..TailDelta::default()
        };
        add_and_flush(&s, &delta);
        assert_eq!(s.counters().rows_skipped, 4);
        let stored = rows(&*store);
        let get = |name: &str| {
            let key = format!("{TAIL_PREFIX}{name}");
            stored
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("backfilled"), Some(newer.to_vec()));
        assert_eq!(get("started_on_marked"), Some(row(1, 5)));
        assert_eq!(get("a_later_field"), Some(vec![3]));
    });
}

/// Asserts that a store that cannot be written costs the counters only: the failed flush
/// is an error and is counted, the delta waits and lands once at the next flush that
/// can write; and that a store that cannot be read fails the load with the store's error,
/// and a flush, whose counts then wait.
/// Catches: a failed write that loses its delta or applies it twice, a failure no counter
/// shows, and a load failure read as zero counters stored.
#[test]
fn a_store_that_cannot_be_written_costs_counters_only() {
    on_both("down", |store| {
        let s = sink(&store);
        store.fail_batch.store(true, Ordering::SeqCst);
        s.add(&every());
        assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
        assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
        assert_eq!(s.counters().writes_failed, 2);
        assert_eq!(s.pending(), every());
        store.fail_batch.store(false, Ordering::SeqCst);
        s.flush().expect("the store is back");
        assert_eq!(s.pending(), TailDelta::default());
        assert_eq!(sink(&store).load(), Ok(totals(&every())));

        store.fail_scan.store(true, Ordering::SeqCst);
        s.add(&every());
        assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
        assert_eq!(s.pending(), every());
        assert_eq!(s.counters().writes_failed, 3);
        assert_eq!(s.load(), Err(TailSinkError::Store(down())));
    });
}

/// Asserts that a flush with nothing waiting asks nothing of the store. Catches: a write
/// on every flush, which would grow the change feed with no count changed.
#[test]
fn a_flush_with_nothing_waiting_writes_nothing() {
    on_both("idle", |store| {
        let s = sink(&store);
        s.flush().expect("nothing to write");
        s.add(&TailDelta::default());
        s.flush().expect("nothing to write");
        assert_eq!(store.txns.load(Ordering::SeqCst), 0);
        assert!(rows(&*store).is_empty());
    });
}

/// Asserts that every stored counter saturates at the end of its range: a count at its
/// largest value stays there, and the sum stops at either end. Catches: a count or a sum
/// that wraps, which would turn a long-running total into a small or negative number.
#[test]
fn stored_counters_saturate() {
    on_both("saturate", |store| {
        let s = sink(&store);
        let high = TailDelta {
            started_on_marked: u64::MAX,
            error_sum_ms: i64::MAX,
            ..TailDelta::default()
        };
        add_and_flush(&s, &high);
        add_and_flush(&s, &high);
        let loaded = sink(&store).load().expect("load");
        assert_eq!(
            (loaded.started_on_marked, loaded.error_sum_ms),
            (u64::MAX, i64::MAX)
        );
        let low = TailDelta {
            error_sum_ms: i64::MIN,
            ..TailDelta::default()
        };
        for _ in 0..3 {
            add_and_flush(&s, &low);
        }
        assert_eq!(sink(&store).load().expect("load").error_sum_ms, i64::MIN);
    });
}

/// Asserts that deltas merge and totals add, counter by counter, saturating. Catches: a
/// merge that keeps only the last delta, and a field left out of the sum.
#[test]
fn deltas_merge_and_totals_add_by_counter() {
    let mut d = every();
    d.merge(&every());
    let mut t = totals(&every());
    t.add(&every());
    assert_eq!(TailTotals::default().error_buckets.len(), TAIL_BUCKETS);
    assert_eq!(d.error_buckets, [2, 4, 6, 8, 10, 12, 14, 16]);
    assert_eq!(t.error_buckets, d.error_buckets);
    assert_eq!((d.started_on_marked, d.error_sum_ms), (6, -80_000));
    assert_eq!(t, totals(&d));
    assert!(!d.is_empty());
    assert!(TailDelta::default().is_empty());
}

/// Asserts that the sink that keeps nothing loads zero, takes every delta and counts
/// nothing. Catches: a default sink that fails (a registry with no store would then report
/// a load failure) or that counts a fault on every delta.
#[test]
fn the_sink_that_keeps_nothing_loads_zero_and_takes_every_delta() {
    assert_eq!(NoTail.load(), Ok(TailTotals::default()));
    NoTail.add(&every());
    assert_eq!(NoTail.counters(), TailSinkCounters::default());
}
