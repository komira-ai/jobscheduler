//! The adapter over the in-memory store, with a wrapper that fails on cue.

use std::sync::atomic::{AtomicBool, Ordering};

use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, StoreError,
    StoreTime, Txn, Versioned,
};
use js_types::LeaseId;

use super::*;
use crate::usage::{DropReason, Observed};
use js_types::sched::usage::CurveSource;
use js_usage::RunMinute;

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
    let clock = Arc::new(ManualClock::default());
    let fail = Arc::new(AtomicBool::new(false));
    let inner = Fake::with_flaw(clock, Flaw::None);
    let store = Flaky {
        inner,
        fail: fail.clone(),
        scan_fails: false,
    };
    (NodeUsage::over(Arc::new(store), 1 << 20, 0).unwrap(), fail)
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

/// Asserts that the profile name admission looks a curve up by is the pinned name of the
/// action key, and that three clean runs of a key give it a curve admission can read
/// while two do not. It catches a mapping that differs between the writer and the reader
/// of curves, and a curve offered too early.
#[test]
fn three_clean_runs_give_the_action_key_a_curve() {
    let (sink, _) = sink();
    let key = key_map(&work()).unwrap();
    assert_eq!(key.as_str(), profile_key(&work()));
    let curves = sink.curves().unwrap();
    finished(&sink, &key, 10);
    finished(&sink, &key, 11);
    assert!(curves.curve(&work()).is_none());
    finished(&sink, &key, 12);
    assert!(curves.curve(&work()).is_some());
}

/// Asserts that the daily prune tells the curves the day: a key idle for more than the
/// staleness limit has no curve afterwards. It catches curves that never learn the day
/// and so never go stale.
#[test]
fn the_prune_moves_the_curves_to_the_day() {
    let (sink, _) = sink();
    let key = key_map(&work()).unwrap();
    for minute in 0..3 {
        finished(&sink, &key, minute);
    }
    let curves = sink.curves().unwrap();
    assert!(curves.curve(&work()).is_some());
    sink.prune(1440 * (js_usage::STALE_DAYS + 5)).unwrap();
    assert!(curves.curve(&work()).is_none());
}

/// Asserts that a start whose usage file cannot be opened is refused with the flag that
/// names the file, and so is one whose stored profiles cannot be read. It catches a
/// server that starts without usage after the operator asked for it, and a refusal
/// that names another flag.
#[test]
fn a_store_that_cannot_be_opened_or_read_refuses_the_start() {
    let setup = StoreSetup {
        // A directory under the target directory: the lock file of a state file sits beside
        // it under its name, and one beside the crate directory would stray into the workspace.
        db: PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
        profile_store_bytes: 1 << 20,
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
