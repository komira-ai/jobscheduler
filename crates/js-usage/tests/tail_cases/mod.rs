//! The tail counter scenarios, written once over a store factory and run by `tail.rs` on the
//! in-memory fake and on SQLite, and by `postgres_tail.rs` on Postgres (the `state-postgres`
//! CI job). Every scenario names the defect it would catch. The conflict, failure and
//! concurrency arms are driven by a store that delegates and fails, lets another writer in,
//! or runs a callback on cue ([`Hooked`]). Threads meet on channels; nothing sleeps and no
//! scenario reads a clock.

use std::any::Any;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use js_state::{
    Changes, Commit, CommitStats, Cond, Conflict, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};
use js_types::HostId;
use js_usage::{
    NodeMinute, NodeSeries, ProfileCurves, ProfileKey, ProfileStore, RunMinute, RunSeries,
    StoreTailSink, TAIL_BUCKETS, TAIL_PREFIX, TAIL_WRITE_TRIES, TailDelta, TailFlush, TailRowError,
    TailSink, TailSinkCounters, TailSinkError, TailTotals,
};

/// The edges the sinks under test count by (the registry's `ERROR_EDGES_MS`).
pub const EDGES: [i64; TAIL_BUCKETS - 1] = [-300_000, -60_000, -10_000, 0, 10_000, 60_000, 300_000];

/// Edges other than [`EDGES`].
const OTHER: [i64; TAIL_BUCKETS - 1] = [-1, 0, 1, 2, 3, 4, 5];

/// The key of the edges row.
const EDGES_KEY: &str = "usage/tail/error_edges_ms";

/// The key of the `started_on_marked` row.
const STARTED: &str = "usage/tail/started_on_marked";

/// A callback a [`Hooked`] runs once, at the next transaction, before it is applied; an
/// error it returns fails the transaction.
type BatchHook = Box<dyn FnMut() -> Option<StoreError> + Send>;

/// A store that delegates, and can fail a scan or a transaction, write a row through the
/// store underneath right after a scan (a second writer that got in first), or run a
/// callback at the next transaction.
pub struct Hooked {
    inner: Arc<dyn Store>,
    fail_scan: AtomicBool,
    fail_batch: AtomicBool,
    /// One write is made after each scan, in order, while any are left.
    after_scan: Mutex<VecDeque<(String, Vec<u8>)>>,
    /// Run once, at the next transaction.
    on_batch: Mutex<Option<BatchHook>>,
    /// Transactions asked of the store.
    txns: AtomicUsize,
    /// What the store lives in (a scratch directory, a database schema), dropped after
    /// the store, which is declared first.
    _keep: Option<Box<dyn Any + Send + Sync>>,
}

impl Hooked {
    /// `inner`, which lives in `keep` when it lives in something.
    pub fn over(inner: Arc<dyn Store>, keep: Option<Box<dyn Any + Send + Sync>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_scan: AtomicBool::new(false),
            fail_batch: AtomicBool::new(false),
            after_scan: Mutex::new(VecDeque::new()),
            on_batch: Mutex::new(None),
            txns: AtomicUsize::new(0),
            _keep: keep,
        })
    }

    fn after_scan(&self, key: &str, value: Vec<u8>) {
        let mut queue = self.after_scan.lock().expect("lock");
        queue.push_back((key.to_owned(), value));
    }

    fn on_batch(&self, hook: impl FnMut() -> Option<StoreError> + Send + 'static) {
        *self.on_batch.lock().expect("lock") = Some(Box::new(hook));
    }
}

fn down() -> StoreError {
    StoreError::Failed("down".to_owned())
}

impl Store for Hooked {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.txns.fetch_add(1, Ordering::SeqCst);
        // Taken out first, so the hook runs without the lock and runs once.
        let hook = self.on_batch.lock().expect("lock").take();
        if let Some(error) = hook.and_then(|mut hook| hook()) {
            return Err(error);
        }
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
            put(&*self.inner, &key, &value);
        }
        Ok(rows)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
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

/// Puts `value` under `key` unconditionally.
fn put(store: &dyn Store, key: &str, value: &[u8]) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

fn sink(store: &Arc<Hooked>) -> StoreTailSink {
    StoreTailSink::new(store.clone(), EDGES)
}

/// A delta in which every counter differs, the sum negative.
pub fn every() -> TailDelta {
    TailDelta {
        started_on_marked: 3,
        backfilled: 2,
        ended_estimated: 5,
        ended_unestimated: 7,
        error_sum_ms: -40_000,
        error_buckets: [1, 2, 3, 4, 5, 6, 7, 8],
    }
}

/// One backfilled start.
fn one_backfill() -> TailDelta {
    TailDelta {
        started_on_marked: 1,
        backfilled: 1,
        ..TailDelta::default()
    }
}

/// The totals of `delta` added to nothing.
pub fn totals(delta: &TailDelta) -> TailTotals {
    let mut t = TailTotals::default();
    t.add(delta);
    t
}

fn add_and_flush(sink: &StoreTailSink, delta: &TailDelta) {
    sink.add(delta);
    assert_eq!(sink.flush(), Ok(TailFlush::Written));
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

/// One `#[test]` per scenario, each over a fresh store from `$open`.
#[macro_export]
macro_rules! tail_store_tests {
    ($open:path) => {
        $crate::tail_store_tests!(@each $open;
            every_counter_is_stored_in_its_own_row_and_read_back,
            tail_counter_deltas_from_two_writers_both_land,
            a_conflicting_write_is_retried_not_lost,
            a_write_beaten_on_every_try_waits_for_the_next_flush,
            tail_counters_with_other_bucket_edges_are_refused_and_not_misread,
            an_edges_row_that_does_not_read_refuses_the_set,
            counter_rows_with_no_edges_row_are_refused_and_left,
            a_newer_row_version_is_skipped_not_fatal,
            a_store_that_cannot_be_written_costs_counters_only,
            a_flush_with_nothing_waiting_writes_nothing,
            stored_counters_saturate,
            a_flush_waits_for_one_under_way_and_writes_what_it_put_back,
            counts_added_during_a_write_wait_for_the_next_flush,
            a_conflict_on_one_row_leaves_no_part_of_a_delta_written,
            edges_written_between_a_read_and_a_write_refuse_it,
            tail_rows_leave_the_node_and_profile_rows_as_they_read,
        );
    };
    (@each $open:path; $($name:ident,)*) => {
        $(
            #[test]
            fn $name() {
                $crate::tail_cases::$name(&$open);
            }
        )*
    };
}

/// Asserts that every counter is written and read back, each in its own row named by
/// its field, with the edges beside them, and that a new sink over the same store (a
/// restart) reads what an earlier one wrote. Catches: a counter that is never written, a
/// swapped field, a sum read back unsigned, and a sink that writes no edges.
pub fn every_counter_is_stored_in_its_own_row_and_read_back(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
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
}

/// Asserts that two sinks over one store each add their own deltas and the stored total
/// is the sum of all of them, interleaved. Catches: a write that sets a value read at
/// open (or at the sink's last write) instead of adding to the row as it is, which would
/// drop the other writer's counts.
pub fn tail_counter_deltas_from_two_writers_both_land(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
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
}

/// Asserts that when another writer changes a row between this one's read and its
/// conditional write, the write is read again and tried again, and lands once: the
/// other writer's 10 and this one's 1 make 11. Catches: an unconditional write (10
/// lost: 1), a conflict given up on (the delta lost: 10), and a delta applied twice (12).
pub fn a_conflicting_write_is_retried_not_lost(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    put(&*store, EDGES_KEY, &edges_row(&EDGES));
    let setup = store.txns.load(Ordering::SeqCst);
    let s = sink(&store);
    store.after_scan("usage/tail/started_on_marked", row(1, 10));
    let one = TailDelta {
        started_on_marked: 1,
        ..TailDelta::default()
    };
    add_and_flush(&s, &one);
    assert_eq!(store.txns.load(Ordering::SeqCst) - setup, 2);
    let loaded = sink(&store).load().expect("load");
    assert_eq!(loaded.started_on_marked, 11);
    assert_eq!(s.pending(), TailDelta::default());
}

/// Asserts that a write another writer beats on every try gives up with a conflict after
/// [`TAIL_WRITE_TRIES`] tries, keeps its delta for the next flush and counts the failure,
/// and that the next flush lands it once. Catches: a retry loop with no end, and a delta
/// dropped (or doubled) after a write that gave up.
pub fn a_write_beaten_on_every_try_waits_for_the_next_flush(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    put(&*store, EDGES_KEY, &edges_row(&EDGES));
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
    assert_eq!(s.flush(), Ok(TailFlush::Written));
    let loaded = sink(&store).load().expect("load");
    assert_eq!(loaded.backfilled, TAIL_WRITE_TRIES as u64 + 1);
}

/// Asserts that a stored set bucketed by other edges is refused with the typed error and
/// zero counters, that a write beside it is refused and dropped, and that the stored rows
/// are byte for byte as they were. Catches: buckets of other ranges read as this build's
/// (a wrong histogram), and a sink that adds to them or rewrites the edges.
pub fn tail_counters_with_other_bucket_edges_are_refused_and_not_misread(
    open: &dyn Fn() -> Arc<Hooked>,
) {
    let store = open();
    let other = [-1, 0, 1, 2, 3, 4, 5];
    put(&*store, EDGES_KEY, &edges_row(&other));
    put(&*store, "usage/tail/error_bucket_0", &row(1, 9));
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
}

/// Asserts that an edges row that does not read (empty, cut short before its count,
/// a length that does not match its count, a newer version) refuses the set the same way:
/// a typed error naming the row, and nothing written. Catches: a broken edges row taken
/// as matching edges, and one overwritten by a write.
pub fn an_edges_row_that_does_not_read_refuses_the_set(open: &dyn Fn() -> Arc<Hooked>) {
    let cases: [(Vec<u8>, TailRowError); 4] = [
        (Vec::new(), TailRowError::Empty),
        (vec![1], TailRowError::Length(1)),
        (vec![1, 7, 0], TailRowError::Length(3)),
        (vec![2, 0], TailRowError::Version(2)),
    ];
    for (bytes, reason) in cases {
        let store = open();
        put(&*store, EDGES_KEY, &bytes);
        let s = sink(&store);
        let unreadable = TailSinkError::Unreadable {
            key: EDGES_KEY.to_owned(),
            reason,
        };
        assert_eq!(s.load(), Err(unreadable.clone()));
        s.add(&every());
        assert_eq!(s.flush(), Err(unreadable));
        assert_eq!(rows(&*store), vec![(EDGES_KEY.to_owned(), bytes)]);
    }
}

/// Asserts that a counter row of a newer version (or one that does not read as this
/// version: empty, or the wrong length) is skipped and counted at a load and at a write,
/// read as zero, never overwritten, while the other counters load and are written as
/// usual; and that a row of a field this build does not know is left alone. Catches: a
/// newer row that fails the whole load, one read as this version's value, one replaced by
/// an older build's write, and a later build's field taken for a counter.
pub fn a_newer_row_version_is_skipped_not_fatal(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    put(&*store, EDGES_KEY, &edges_row(&EDGES));
    let newer = [2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 9];
    put(&*store, "usage/tail/backfilled", &newer);
    put(&*store, "usage/tail/ended_estimated", &[]);
    put(&*store, "usage/tail/ended_unestimated", &[1, 1]);
    put(&*store, "usage/tail/started_on_marked", &row(1, 4));
    put(&*store, "usage/tail/a_later_field", &[3]);
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
}

/// Asserts that a store that cannot be written costs the counters only: the failed flush
/// is an error and is counted, the delta waits and lands once at the next flush that
/// can write; and that a store that cannot be read fails the load with the store's error,
/// and a flush, whose counts then wait.
/// Catches: a failed write that loses its delta or applies it twice, a failure no counter
/// shows, and a load failure read as zero counters stored.
pub fn a_store_that_cannot_be_written_costs_counters_only(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    let s = sink(&store);
    store.fail_batch.store(true, Ordering::SeqCst);
    s.add(&every());
    assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
    assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
    assert_eq!(s.counters().writes_failed, 2);
    assert_eq!(s.pending(), every());
    store.fail_batch.store(false, Ordering::SeqCst);
    assert_eq!(s.flush(), Ok(TailFlush::Written));
    assert_eq!(s.pending(), TailDelta::default());
    assert_eq!(sink(&store).load(), Ok(totals(&every())));

    store.fail_scan.store(true, Ordering::SeqCst);
    s.add(&every());
    assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
    assert_eq!(s.pending(), every());
    assert_eq!(s.counters().writes_failed, 3);
    assert_eq!(s.load(), Err(TailSinkError::Store(down())));
}

/// Asserts that a flush with nothing waiting asks nothing of the store. Catches: a write
/// on every flush, which would grow the change feed with no count changed.
pub fn a_flush_with_nothing_waiting_writes_nothing(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    let s = sink(&store);
    assert_eq!(s.flush(), Ok(TailFlush::Idle));
    s.add(&TailDelta::default());
    assert_eq!(s.flush(), Ok(TailFlush::Idle));
    assert_eq!(store.txns.load(Ordering::SeqCst), 0);
    assert!(rows(&*store).is_empty());
}

/// Asserts that every stored counter saturates at the end of its range: a count at its
/// largest value stays there, and the sum stops at either end. Catches: a count or a sum
/// that wraps, which would turn a long-running total into a small or negative number.
pub fn stored_counters_saturate(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
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
}

/// Asserts that counter rows stored with no edges row are refused with the typed error and
/// zero counters, that a write beside them is refused and dropped, and that the rows are
/// as they were; a row of a field this build does not know is not a counter row, so beside
/// it alone the set is empty and is written. Catches: buckets whose ranges are unknown read
/// as this build's, or labelled with this build's edges by a later write.
pub fn counter_rows_with_no_edges_row_are_refused_and_left(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    put(&*store, "usage/tail/error_bucket_0", &row(1, 9));
    let before = rows(&*store);
    let s = sink(&store);
    assert_eq!(s.load(), Err(TailSinkError::NoEdges));
    s.add(&every());
    assert_eq!(s.flush(), Err(TailSinkError::NoEdges));
    assert_eq!(s.pending(), TailDelta::default());
    assert_eq!(s.counters().writes_failed, 1);
    assert_eq!(rows(&*store), before);

    let later = open();
    put(&*later, "usage/tail/a_later_field", &[3]);
    let s = sink(&later);
    assert_eq!(s.load(), Ok(TailTotals::default()));
    add_and_flush(&s, &every());
    assert_eq!(sink(&later).load(), Ok(totals(&every())));
}

/// Asserts that a flush that starts while another is writing waits for it: the first
/// write fails and puts its counts back, and the second (the final flush at shutdown)
/// writes them, so a sink opened afterwards reads them. The first flush is held inside
/// its transaction until the second has been started. Catches: a final flush that returns
/// with nothing written while an earlier flush still holds counts it then puts back, which
/// loses them from the store although the shutdown flushed.
pub fn a_flush_waits_for_one_under_way_and_writes_what_it_put_back(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    let s = Arc::new(sink(&store));
    let (entered, writing) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    store.on_batch(move || {
        entered.send(()).expect("the test waits for the write");
        released.recv().expect("the test releases the write");
        Some(down())
    });
    s.add(&one_backfill());
    let first = Arc::clone(&s);
    let first = thread::spawn(move || first.flush());
    writing.recv().expect("the first flush is writing");
    let (starting, started) = mpsc::channel();
    let last = Arc::clone(&s);
    let last = thread::spawn(move || {
        starting.send(()).expect("the test waits for the start");
        last.flush()
    });
    started.recv().expect("the final flush has started");
    release.send(()).expect("the first flush is waiting");
    assert_eq!(
        first.join().expect("no panic"),
        Err(TailSinkError::Store(down()))
    );
    assert_eq!(last.join().expect("no panic"), Ok(TailFlush::Written));
    assert_eq!(s.pending(), TailDelta::default());
    assert_eq!(sink(&store).load(), Ok(totals(&one_backfill())));
}

/// Asserts that counts added while a write is under way wait for the next flush, whether
/// the write lands or fails, and that a failed write puts its own counts back beside them
/// once: after a landed write the store holds only what it carried; after a failed one
/// both wait, and the next flush writes each once. Catches: counts added during a write
/// dropped by it, or written by it although it read the rows before they came; and a
/// failed write that puts its counts back over (or twice beside) the ones added meanwhile.
pub fn counts_added_during_a_write_wait_for_the_next_flush(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    let s = Arc::new(sink(&store));
    let one = one_backfill();
    let two = TailDelta {
        ended_unestimated: 2,
        ..TailDelta::default()
    };
    let during = Arc::clone(&s);
    store.on_batch(move || {
        during.add(&two);
        None
    });
    s.add(&one);
    assert_eq!(s.flush(), Ok(TailFlush::Written));
    assert_eq!(s.pending(), two);
    assert_eq!(sink(&store).load(), Ok(totals(&one)));

    let during = Arc::clone(&s);
    store.on_batch(move || {
        during.add(&one);
        Some(down())
    });
    assert_eq!(s.flush(), Err(TailSinkError::Store(down())));
    let mut both = two;
    both.merge(&one);
    assert_eq!(s.pending(), both);
    assert_eq!(s.flush(), Ok(TailFlush::Written));
    let mut all = totals(&one);
    all.add(&both);
    assert_eq!(sink(&store).load(), Ok(all));
}

/// Asserts that a delta of many counters is one transaction: while another writer changes
/// one of its rows under every try, no other row is written, and once a try gets through,
/// every counter lands, the other writer's change included. Catches: a write that commits
/// row by row, leaving part of a delta in the store when one row conflicts.
pub fn a_conflict_on_one_row_leaves_no_part_of_a_delta_written(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    put(&*store, EDGES_KEY, &edges_row(&EDGES));
    let s = sink(&store);
    for n in 0..TAIL_WRITE_TRIES {
        store.after_scan(STARTED, row(1, n as u64 + 1));
    }
    s.add(&every());
    let tries = TAIL_WRITE_TRIES;
    assert_eq!(s.flush(), Err(TailSinkError::Conflict { tries }));
    let expected = vec![
        (EDGES_KEY.to_owned(), edges_row(&EDGES)),
        (STARTED.to_owned(), row(1, TAIL_WRITE_TRIES as u64)),
    ];
    assert_eq!(rows(&*store), expected);

    store.after_scan(STARTED, row(1, 100));
    assert_eq!(s.flush(), Ok(TailFlush::Written));
    let mut expected = totals(&every());
    expected.started_on_marked += 100;
    assert_eq!(sink(&store).load(), Ok(expected));
}

/// Asserts that edges another writer stores between this write's read and its commit
/// refuse the write: edges first written meanwhile, and edges replaced meanwhile. The
/// write is tried again, finds the other edges, and writes nothing; the rows are the other
/// writer's. Catches: counts of one set of ranges added to buckets of another, through a
/// write that does not hold the edges it read.
pub fn edges_written_between_a_read_and_a_write_refuse_it(open: &dyn Fn() -> Arc<Hooked>) {
    let refused = TailSinkError::OtherEdges {
        stored: OTHER.to_vec(),
        build: EDGES.to_vec(),
    };
    let created = open();
    let s = sink(&created);
    created.after_scan(EDGES_KEY, edges_row(&OTHER));
    s.add(&every());
    assert_eq!(s.flush(), Err(refused.clone()));
    let only_edges = vec![(EDGES_KEY.to_owned(), edges_row(&OTHER))];
    assert_eq!(rows(&*created), only_edges);

    let replaced = open();
    put(&*replaced, EDGES_KEY, &edges_row(&EDGES));
    put(&*replaced, "usage/tail/backfilled", &row(1, 5));
    let s = sink(&replaced);
    replaced.after_scan(EDGES_KEY, edges_row(&OTHER));
    s.add(&every());
    assert_eq!(s.flush(), Err(refused));
    let expected = vec![
        ("usage/tail/backfilled".to_owned(), row(1, 5)),
        (EDGES_KEY.to_owned(), edges_row(&OTHER)),
    ];
    assert_eq!(rows(&*replaced), expected);
}

/// Asserts the compatibility of the new key space with the store as an earlier release
/// left it: a store holding only node and profile rows (what a release before the tail
/// counters wrote) loads as zero counters and takes them; and the tail rows written beside
/// those rows leave the node series and the profile store reading exactly what they read
/// before (an earlier release reads only those two prefixes). Catches: tail rows under a
/// prefix an older reader scans, or a sink that takes another key space's rows for its
/// own.
pub fn tail_rows_leave_the_node_and_profile_rows_as_they_read(open: &dyn Fn() -> Arc<Hooked>) {
    let store = open();
    let host = HostId::new("h").expect("a host id");
    let minute = NodeMinute {
        minute: 120,
        cpu_millis: 5,
        ws_max_bytes: 6,
        leases: 1,
    };
    let series = NodeSeries::new(&*store);
    series
        .append(&host, &[minute])
        .expect("the minute is written");
    let curves = || Arc::new(ProfileCurves::new(Box::new(|_| None)));
    let mut profiles = ProfileStore::open(store.clone(), curves(), 1 << 20, 0).expect("open");
    let key = ProfileKey::parse("k").expect("a key");
    let run = |minute| RunMinute {
        minute,
        cores_milli: 1000,
        ws_bytes: 1 << 20,
    };
    let runs = RunSeries {
        minutes: (0..3).map(run).collect(),
        flags: 0,
    };
    profiles
        .run_ended(&key, &runs, 0)
        .expect("the run is written");
    let before = profiles.profile(&key).expect("read");

    let s = sink(&store);
    assert_eq!(s.load(), Ok(TailTotals::default()));
    add_and_flush(&s, &every());

    let limit = NonZeroUsize::new(10).expect("nonzero");
    assert_eq!(series.range(&host, 0, 1000, limit), Ok(vec![minute]));
    let reopened = ProfileStore::open(store.clone(), curves(), 1 << 20, 0).expect("open");
    assert_eq!(reopened.corrupt_rows(), 0);
    assert_eq!(reopened.len(), profiles.len());
    assert_eq!(reopened.profile(&key).expect("read"), before);
}
