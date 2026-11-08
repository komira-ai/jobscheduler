//! The adapter over the in-memory store, with a wrapper that fails on cue.

use std::sync::atomic::{AtomicBool, Ordering};

use js_registry::ERROR_EDGES_MS;
use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, StoreError,
    StoreTime, Txn, Versioned,
};
use js_types::LeaseId;

use super::*;
use crate::usage::{DropReason, Observed};
use js_types::sched::usage::CurveSource;
use js_usage::{AllocationId, DrainOutcome, EndedReason, EndedRow, RunMinute};

/// A store that forwards to the fake and fails every write while `fail` is set.
struct Flaky {
    inner: Fake,
    fail: Arc<AtomicBool>,
    /// Whether the scan of a prefix (how the profile store opens) fails.
    scan_fails: bool,
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        match self.fail.load(Ordering::SeqCst) {
            true => Err(StoreError::Failed("down".to_string())),
            false => self.inner.apply_batch(batch),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if self.scan_fails {
            return Err(StoreError::Failed("down".to_string()));
        }
        self.inner.scan_prefix(prefix)
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

fn sink() -> (NodeUsage, Arc<AtomicBool>) {
    sink_named(no_names())
}

fn sink_named(names: NameMap) -> (NodeUsage, Arc<AtomicBool>) {
    let clock = Arc::new(ManualClock::default());
    let fail = Arc::new(AtomicBool::new(false));
    let inner = Fake::with_flaw(clock, Flaw::None);
    let store = Flaky {
        inner,
        fail: fail.clone(),
        scan_fails: false,
    };
    (
        NodeUsage::over(Arc::new(store), 1 << 20, 0, names).unwrap(),
        fail,
    )
}

fn limits() -> HostLimits {
    HostLimits {
        threads: 8,
        memory_bytes: 1 << 34,
    }
}

fn reading(seq: u64, mono_sec: u64, cpu_sec: u64) -> LoadIn {
    LoadIn {
        seq,
        mono_usec: mono_sec * 1_000_000,
        cpu_usage_usec: cpu_sec * 1_000_000,
        working_set_bytes: 4096,
        actions_cpu_limited: false,
        covered: vec![LeaseId::new(1, 1)],
        minutes: Vec::new(),
        clocks: Vec::new(),
    }
}

fn host() -> HostId {
    HostId::new("h1").unwrap()
}

/// Asserts the path of a reading to a stored row: the first reading only sets a baseline
/// (dropped, counted), the second is accepted, and the flush at a later minute returns the
/// closed minute and writes it, where it reads back with the node's CPU. It catches a
/// flush that returns rows it did not write, and one that writes nothing.
#[test]
fn readings_become_a_stored_node_minute_at_a_flush() {
    let (sink, _) = sink();
    let first = sink.observe(&host(), 1, &reading(1, 10, 10), limits(), 100);
    assert_eq!(first.observed, Observed::Dropped(DropReason::NoBaseline));
    let second = sink.observe(&host(), 1, &reading(2, 20, 15), limits(), 100);
    assert_eq!(second.observed, Observed::Accepted);
    assert!(sink.flush(100).unwrap().is_empty());
    let closed = sink.flush(101).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, host());
    assert_eq!(closed[0].1.minute, 100);
    assert_eq!(closed[0].1.cpu_millis, 500);
    assert_eq!(sink.range(&host(), 0, 200), vec![closed[0].1]);
    assert_eq!(sink.counters().dropped[&DropReason::NoBaseline], 1);
    assert!(sink.flush(102).unwrap().is_empty());
}

/// Asserts that a write that fails keeps the minute: the flush returns the fault, and the
/// next flush, with the store back, writes the minute once. It catches a minute lost to a
/// failed write and one written twice.
#[test]
fn a_failed_write_keeps_the_minute_for_the_next_flush() {
    let (sink, fail) = sink();
    sink.observe(&host(), 1, &reading(1, 10, 10), limits(), 100);
    sink.observe(&host(), 1, &reading(2, 20, 15), limits(), 100);
    fail.store(true, Ordering::SeqCst);
    let err = sink.flush(101).unwrap_err();
    assert!(err.0.contains("down"), "{err}");
    assert!(sink.range(&host(), 0, 200).is_empty());
    fail.store(false, Ordering::SeqCst);
    assert!(sink.flush(102).unwrap().is_empty());
    assert_eq!(sink.range(&host(), 0, 200).len(), 1);
}

/// Asserts that prune succeeds on an empty store, fails with the store's fault when the
/// store does, and that a run handed over is taken and kept for the profile side. It
/// catches a prune that swallows a failure.
#[test]
fn prune_reports_a_failed_store_and_a_run_is_taken() {
    let (sink, fail) = sink();
    sink.observe(&host(), 1, &reading(1, 10, 10), limits(), 100);
    sink.observe(&host(), 1, &reading(2, 20, 15), limits(), 100);
    sink.flush(101).unwrap();
    assert_eq!(sink.prune(101), Ok(()));
    assert_eq!(sink.range(&host(), 0, 200).len(), 1);
    let late = 100 + js_usage::NODE_RETENTION_MINUTES + 120;
    fail.store(true, Ordering::SeqCst);
    assert!(sink.prune(late).is_err());
    fail.store(false, Ordering::SeqCst);
    assert_eq!(sink.range(&host(), 0, 200).len(), 1);
    sink.prune(late).unwrap();
    assert!(sink.range(&host(), 0, 200).is_empty());
    let end = RunEnd {
        key: crate::usage::ProfileKey::parse("k").unwrap(),
        lease: LeaseId::new(1, 1),
        series: Default::default(),
    };
    assert_eq!(sink.run_ended(&end, 0), Ok(()));
}

fn work() -> WorkKey {
    WorkKey::new("Build/Pool-1", "ab12/34")
}

fn finished(sink: &NodeUsage, key: &ProfileKey, minute: u64) {
    let series = RunSeries {
        minutes: vec![RunMinute {
            minute: 0,
            cores_milli: 1500,
            ws_bytes: 1 << 20,
        }],
        flags: 0,
    };
    let end = RunEnd {
        key: key.clone(),
        lease: LeaseId::new(1, 1),
        series,
    };
    sink.run_ended(&end, minute).unwrap();
}

/// A map that gives every work key of class `class` the profile `name`, as a caller who
/// supplied that name for the runs of the work would.
fn named(class: &'static str, name: &'static str) -> NameMap {
    Arc::new(move |key| (key.class == class).then(|| ProfileKey::parse(name).unwrap()))
}

/// Asserts that the profile of a run is the name the caller supplied for it: three clean
/// runs ended under the supplied name give the work keys that name stands for a curve
/// admission can read (two runs do not), and a work key the name does not stand for has
/// none. It catches a server that files the runs under a hash of the work key, so that the
/// supplied name never finds them.
#[test]
fn three_clean_runs_under_a_supplied_name_give_its_work_keys_a_curve() {
    let (sink, _) = sink_named(named("Build/Pool-1", "est.row-1"));
    let key = ProfileKey::parse("est.row-1").unwrap();
    let curves = sink.curves().unwrap();
    finished(&sink, &key, 10);
    finished(&sink, &key, 11);
    assert!(curves.curve(&work()).is_none());
    finished(&sink, &key, 12);
    assert!(curves.curve(&work()).is_some());
    assert!(curves.curve(&WorkKey::new("Other", "ab12/34")).is_none());
}

/// Asserts that two different work keys with one supplied name share one profile: runs
/// of either, ended under that name, teach both. The action digest changes with every
/// input, so a profile that followed the work key would stay cold. It catches a name
/// derived from the whole work key.
#[test]
fn two_work_keys_with_one_supplied_name_share_one_profile() {
    let names: NameMap = Arc::new(|_| Some(ProfileKey::parse("est.row-1").unwrap()));
    let (sink, _) = sink_named(names);
    let key = ProfileKey::parse("est.row-1").unwrap();
    let first = WorkKey::new("Build/Pool-1", "aaaa/1");
    let second = WorkKey::new("Build/Pool-1", "bbbb/2");
    for minute in 0..3 {
        finished(&sink, &key, minute);
    }
    let curves = sink.curves().unwrap();
    assert!(curves.curve(&first).is_some());
    assert!(curves.curve(&second).is_some());
}

/// Asserts that the daily prune tells the curves the day: a key idle for more than the
/// staleness limit has no curve afterwards. It catches curves that never learn the day
/// and so never go stale.
#[test]
fn the_prune_moves_the_curves_to_the_day() {
    let (sink, _) = sink_named(named("Build/Pool-1", "est.row-1"));
    let key = ProfileKey::parse("est.row-1").unwrap();
    for minute in 0..3 {
        finished(&sink, &key, minute);
    }
    let curves = sink.curves().unwrap();
    assert!(curves.curve(&work()).is_some());
    sink.prune(1440 * (js_usage::STALE_DAYS + 5)).unwrap();
    assert!(curves.curve(&work()).is_none());
}

impl NodeUsage {
    /// The stored rows of `host` from minute `from` to `to`.
    fn range(&self, host: &HostId, from: u64, to: u64) -> Vec<NodeRow> {
        self.node_rows(host, from, to, NonZeroUsize::new(100).unwrap())
            .unwrap()
    }
}

/// Asserts the reads of the sink: a key never seen is unknown; after two runs it has a
/// count and no fit, after three a fit whose first point is the runs' own; it is stale
/// once idle past the limit; rows come oldest first inside the asked range and never
/// more than the limit. It catches a fit offered early, a last-used day that is not the
/// run's, and a limit or range that is not applied.
#[test]
fn the_sink_reads_back_rows_and_profiles() {
    let (sink, _) = sink();
    let key = ProfileKey::parse("est.row-1").unwrap();
    assert_eq!(sink.key_profile(&key), Ok(None));
    finished(&sink, &key, 10);
    finished(&sink, &key, 11);
    let two = sink.key_profile(&key).unwrap().unwrap();
    assert_eq!((two.runs, two.fit.is_none()), (2, true));
    finished(&sink, &key, 12);
    let three = sink.key_profile(&key).unwrap().unwrap();
    let fit = three.fit.clone().unwrap();
    assert_eq!((three.runs, fit.runs), (3, 3));
    assert_eq!(fit.points[0].cores_milli, 1500);
    assert!(!three.is_stale(three.last_used_day));
    assert!(three.is_stale(three.last_used_day + js_usage::STALE_DAYS + 1));

    sink.observe(&host(), 1, &reading(1, 10, 10), limits(), 100);
    sink.observe(&host(), 1, &reading(2, 20, 15), limits(), 100);
    sink.observe(&host(), 1, &reading(3, 30, 20), limits(), 101);
    sink.flush(102).unwrap();
    let all = NonZeroUsize::new(100).unwrap();
    assert_eq!(sink.node_rows(&host(), 0, 200, all).unwrap().len(), 2);
    let one = sink.node_rows(&host(), 0, 200, NonZeroUsize::MIN).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].minute, 100);
    let late = sink.node_rows(&host(), 101, 200, all).unwrap();
    assert_eq!(late.iter().map(|r| r.minute).collect::<Vec<_>>(), [101]);
    assert!(sink.node_rows(&host(), 150, 200, all).unwrap().is_empty());
}

/// Asserts that a start whose usage file cannot be opened is refused with the flag that
/// names the file, and so is one whose stored profiles cannot be read. It catches a
/// server that starts without usage after the operator asked for it, and a refusal
/// that names another flag.
#[test]
fn a_store_that_cannot_be_opened_or_read_refuses_the_start() {
    // A directory is not a usage file. It is made inside a directory of this run, which is
    // removed afterwards with the lock file the store makes beside it.
    let scratch = js_testkit::dir::TestDir::under(std::env::temp_dir(), "js-lease-server-usage");
    let dir = scratch.join("not-a-file");
    std::fs::create_dir_all(&dir).unwrap();
    let setup = StoreSetup {
        db: dir,
        profile_store_bytes: 1 << 20,
        history_retention_days: 90,
        names: no_names(),
    };
    let err = open(&setup, 0)
        .err()
        .expect("a directory is not a usage file");
    assert_eq!(err.flag(), "--usage-db");
    let store = Flaky {
        inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
        fail: Arc::new(AtomicBool::new(false)),
        scan_fails: true,
    };
    let err = sink_over(Arc::new(store), &setup, 0)
        .err()
        .expect("a store that cannot be scanned");
    assert_eq!(err.flag(), "--usage-db");
    assert!(err.reason().contains("down"), "{}", err.reason());
}

/// Asserts that the tail counts the registry hands the sink are written at a flush, that a
/// store that cannot be written costs the stored counters only (every flush still
/// succeeds, the counts wait and the failures are counted), and that once the store is
/// back the counts land once. Catches: tail counts that no flush writes, counts lost or
/// doubled across failed writes, and a failed tail write that fails the node flush.
#[test]
fn the_tail_counts_are_written_at_a_flush_and_wait_while_the_store_is_down() {
    let (sink, fail) = sink();
    let tail = sink.tail().unwrap();
    let delta = js_usage::TailDelta {
        backfilled: 1,
        ..js_usage::TailDelta::default()
    };
    tail.add(&delta);
    fail.store(true, Ordering::SeqCst);
    assert!(sink.flush(100).unwrap().is_empty());
    assert!(sink.flush(100).unwrap().is_empty());
    assert_eq!(tail.counters().writes_failed, 2);
    fail.store(false, Ordering::SeqCst);
    sink.flush(100).unwrap();
    sink.flush(100).unwrap();
    let stored = StoreTailSink::new(sink.store.clone(), ERROR_EDGES_MS);
    assert_eq!(stored.load().unwrap().backfilled, 1);
    assert_eq!(tail.counters().writes_failed, 2);
}

/// Asserts what the tail counters' log says over a run of failures: nothing for a flush
/// with nothing waiting; the first failure of a run, by its exact text, once; nothing for
/// the failures after it, nor for a flush with nothing waiting while the store still
/// refuses (the refused counts are dropped); the end of the run once, when a write lands;
/// and the first failure of a later run again. Catches: an idle flush taken for a
/// recovery (a false recovery, then a second warning for the same refusal), a warning on
/// every failed flush, and a recovery that is never said.
#[test]
fn the_tail_log_says_a_run_of_failures_once_and_its_end_once() {
    let (sink, fail) = sink();
    let tail = sink.tail().unwrap();
    let one = js_usage::TailDelta {
        backfilled: 1,
        ..js_usage::TailDelta::default()
    };
    let edges = "usage/tail/error_edges_ms";
    let put = Txn::new().put(edges, vec![9], js_state::Cond::Any);
    sink.store.txn(put).unwrap().unwrap();
    assert_eq!(sink.flush_tail(), None);

    tail.add(&one);
    let unreadable = "tail counter row usage/tail/error_edges_ms does not read: \
                      the row has version 9, which this build does not read";
    let first = sink.flush_tail();
    assert_eq!(first, Some(TailNote::Failing(unreadable.to_owned())));
    first.into_iter().for_each(log_tail);
    assert_eq!(sink.flush_tail(), None);
    tail.add(&one);
    assert_eq!(sink.flush_tail(), None);

    let delete = Txn::new().delete(edges, js_state::Cond::Any);
    sink.store.txn(delete).unwrap().unwrap();
    tail.add(&one);
    let recovered = sink.flush_tail();
    assert_eq!(recovered, Some(TailNote::Recovered));
    recovered.into_iter().for_each(log_tail);
    assert_eq!(sink.flush_tail(), None);

    fail.store(true, Ordering::SeqCst);
    tail.add(&one);
    let down = "tail counters: state store failure: down".to_owned();
    assert_eq!(sink.flush_tail(), Some(TailNote::Failing(down)));
    fail.store(false, Ordering::SeqCst);
    let stored = StoreTailSink::new(sink.store.clone(), ERROR_EDGES_MS);
    assert_eq!(stored.load().unwrap().backfilled, 1);
    assert_eq!(tail.counters().writes_failed, 3);
}

/// Milliseconds in a day.
const DAY_MS: u64 = 86_400_000;

/// The record of lease `1-n`, ended at `end` ms.
fn ended_row(n: u64, end: u64) -> EndedRow {
    EndedRow {
        id: AllocationId::of_lease(&LeaseId::new(1, n)),
        profile: None,
        host: Some(host()),
        booked: None,
        queued_ms: Some(0),
        run_ms: Some(1),
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

fn ids(rows: Result<Vec<EndedRow>, UsageFault>) -> Vec<String> {
    rows.unwrap().iter().map(|row| row.id.to_string()).collect()
}

/// Asserts the history side of the sink the server opens over its SQLite usage file: a
/// record is written once, read back oldest first within the asked range, and the daily
/// prune removes the records that ended before `--history-retention-days` (two here) and
/// keeps the one that ended exactly then. It catches a sink that keeps no history, a
/// record written twice, and a retention not taken from the setup (the default would keep
/// every record here).
#[test]
fn the_usage_file_keeps_ended_allocations_for_the_retention_days() {
    let scratch = js_testkit::dir::TestDir::under(std::env::temp_dir(), "js-lease-server-ended");
    let setup = StoreSetup {
        db: scratch.join("usage.db"),
        profile_store_bytes: 1 << 20,
        history_retention_days: 2,
        names: no_names(),
    };
    let sink = open(&setup, 0).unwrap();
    let now_minute = 10 * MINUTES_PER_DAY;
    let now = now_minute * 60_000;
    let edge = now - 2 * DAY_MS;
    for (n, end) in [(1, edge - 1), (2, edge), (3, now - 1), (3, now - 1)] {
        sink.allocation_ended(&ended_row(n, end)).unwrap();
    }
    let all = NonZeroUsize::new(10).unwrap();
    assert_eq!(
        ids(sink.ended_rows(0, u64::MAX, all)),
        ["1-1", "1-2", "1-3"]
    );
    assert_eq!(ids(sink.ended_rows(edge, edge, all)), ["1-2"]);
    sink.prune(now_minute).unwrap();
    assert_eq!(ids(sink.ended_rows(0, u64::MAX, all)), ["1-2", "1-3"]);
}

/// Asserts that a store that refuses writes fails a record's write and the prune of old
/// records with the store's fault, and that the prune removes them once the store is back:
/// by default after 90 days. It catches a failure swallowed as success.
#[test]
fn a_store_that_refuses_writes_fails_the_history_write_and_prune() {
    let (sink, fail) = sink();
    sink.allocation_ended(&ended_row(1, 0)).unwrap();
    fail.store(true, Ordering::SeqCst);
    let err = sink.allocation_ended(&ended_row(2, 5)).unwrap_err();
    assert!(err.0.contains("down"), "{err}");
    let day_90 = 90 * MINUTES_PER_DAY;
    assert!(sink.prune(day_90 + 1).is_err());
    fail.store(false, Ordering::SeqCst);
    let all = NonZeroUsize::new(10).unwrap();
    sink.prune(day_90).unwrap();
    assert_eq!(ids(sink.ended_rows(0, u64::MAX, all)), ["1-1"]);
    sink.prune(day_90 + 1).unwrap();
    assert!(sink.ended_rows(0, u64::MAX, all).unwrap().is_empty());
}

/// Asserts that a page of the history is read from the store the records were written to:
/// two of three records on the first page with the cursor of the second, and the third
/// after it with no cursor. It catches a sink that answers the empty page a sink without
/// history answers.
#[test]
fn a_page_of_the_history_is_read_from_the_store() {
    let (sink, _) = sink();
    for n in 1..=3 {
        sink.allocation_ended(&ended_row(n, n * 10)).unwrap();
    }
    let query = js_usage::EndedQuery {
        after: None,
        from_ms: 0,
        before_ms: None,
        host: None,
        id: None,
        limit: NonZeroUsize::new(2).unwrap(),
    };
    let first = sink.ended_rows_after(&query).unwrap();
    assert_eq!(ids(Ok(first.rows.clone())), ["1-1", "1-2"]);
    let next = js_usage::EndedQuery {
        after: first.next,
        ..query
    };
    let second = sink.ended_rows_after(&next).unwrap();
    assert_eq!(ids(Ok(second.rows)), ["1-3"]);
    assert_eq!(second.next, None);
}
