//! The tail counters kept in a state store through `js_usage::StoreTailSink`: a registry
//! started again over the same store counts on from what the first wrote, and a store
//! that cannot be written costs the counters only. Every store-backed test runs over the
//! in-memory fake and over a real SQLite file. The registry is driven on a manual clock;
//! nothing sleeps and no test asserts how long anything took.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use js_pool_static::Candidate;
use js_provider::{Pool, StartSpec};
use js_registry::{
    ERROR_EDGES_MS, MarkedTails, Placer, PoolConfig, ProfileLives, TailCounters, WholeNodeConfig,
};
use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use js_testkit::dir::TestDir;
use js_types::{Admission, ClassId, Dims, ProviderId, Request, ResourceClass, Resources};
use js_usage::{ProfileKey, StoreTailSink, TailSink, TailSinkError};
use support::world::*;
use support::*;

/// Runs `test` over the in-memory fake and then over a real SQLite file in a scratch
/// directory, each fresh and empty.
fn on_both(name: &str, test: impl Fn(Arc<dyn Store>)) {
    test(Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )));
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-registry-{name}"));
    let clock = Arc::new(ManualClock::default());
    let sqlite = SqliteStore::open(&dir.path().join("state.db"), clock, Options::default())
        .expect("open the scratch store");
    test(Arc::new(sqlite));
}

/// A class that books part of a host, so its leases share one and may backfill.
fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// A `book` lease of one core that may run `wall_s`.
fn book_spec(sequence: u64, wall_s: u64) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        max_wall: Duration::from_secs(wall_s),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// The pool: whole-node leases, and `book` leases that share a host.
fn pool() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, whole(), pool_shape(), 4)
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .unwrap()
        .with_share_class(book())
        .unwrap()
}

/// A placer that books the first host it is shown, a host set aside included.
struct First;

impl Placer for First {
    fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
        Some(0)
    }
}

/// Every profile's runs take two minutes at the P90.
struct TwoMinutes;

impl ProfileLives for TwoMinutes {
    fn wall_minutes_p90(&self, _: &ProfileKey) -> Option<u32> {
        Some(2)
    }
}

/// A registry over host `a` whose tail counters go to `sink`.
fn world(sink: Arc<dyn TailSink>) -> World {
    let mut w = World::of(pool(), &["a"]);
    w.fx.registry.set_placer(Arc::new(First));
    w.fx.registry.set_lives(Arc::new(TwoMinutes));
    w.provider = w.fx.registry.provider();
    w.fx.registry.set_tail_sink(sink).unwrap();
    w
}

/// Drives host `a` through one backfilled start and two ends: `book` lease 1 starts with a
/// keyed run estimated at two minutes; whole-node request `q` marks `a`; `book` lease 3
/// starts on the marked host as backfill; two and a half minutes on, lease 1 ends 30 s
/// late and lease 3, which has no estimate, ends too. The provider is called directly,
/// not through the world's checks, which take any lease that appears on a marked host
/// for a fault.
fn drive(w: &mut World) {
    assert_eq!(
        started_on(&w.provider.start(book_spec(1, 3_600))),
        Some("a")
    );
    let key = ProfileKey::parse("two").unwrap();
    w.fx.registry
        .queue_keyed_run(&host("a"), &lease(1), job("r1"), Some(key))
        .unwrap();
    w.provider.start(World::whole_spec(2, Some("q"), 600));
    assert_eq!(w.reserved_host("q").as_deref(), Some("a"));
    assert_eq!(started_on(&w.provider.start(book_spec(3, 60))), Some("a"));
    w.fx.clock.advance(Duration::from_millis(150_000));
    w.provider.stop(&lease(1));
    w.provider.stop(&lease(3));
}

/// What [`drive`] counts.
fn driven() -> TailCounters {
    let mut buckets = [0; 8];
    buckets[ERROR_EDGES_MS.partition_point(|edge| *edge < 30_000)] = 1;
    TailCounters {
        started_on_marked: 1,
        backfilled: 1,
        ended_estimated: 1,
        ended_unestimated: 1,
        error_sum_ms: 30_000,
        error_buckets: buckets,
    }
}

/// Asserts that every tail counter survives a restart: a registry started over the store
/// the first one's sink wrote reports the same counters, bucket by bucket, and a third
/// started after the second has flushed reports them again. Catches: a counter kept in
/// memory only (it would read zero after the restart), and a restart that hands the
/// loaded totals to the sink again as new counts (the third would read double).
#[test]
fn tail_counters_survive_a_restart() {
    on_both("tail-restart", |store| {
        let sink = Arc::new(StoreTailSink::new(store.clone(), ERROR_EDGES_MS));
        let mut first = world(sink.clone());
        drive(&mut first);
        let before = first.fx.registry.tail_counters();
        assert_eq!(before, driven());
        sink.flush().unwrap();

        let again = Arc::new(StoreTailSink::new(store.clone(), ERROR_EDGES_MS));
        let second = world(again.clone());
        assert_eq!(second.fx.registry.tail_counters(), before);
        again.flush().unwrap();
        let third = world(Arc::new(StoreTailSink::new(store, ERROR_EDGES_MS)));
        assert_eq!(third.fx.registry.tail_counters(), before);
    });
}

/// A store over the fake that can be told to fail every transaction.
struct Failing {
    inner: Fake,
    down: AtomicBool,
}

impl Store for Failing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        match self.down.load(Ordering::SeqCst) {
            true => Err(StoreError::Failed("down".to_owned())),
            false => self.inner.apply_batch(batch),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
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

/// Asserts that a store that cannot be written costs the counters only: every start and
/// stop still succeeds, the registry's counters still hold every count, the failed write
/// is counted by the sink, and once the store is back the counts land, once. Catches: a
/// start or stop that fails (or is refused) because the sink could not write, counts
/// dropped from memory with the failed write, a failure no counter shows, and counts
/// written twice after the store comes back.
#[test]
fn a_store_that_cannot_be_written_costs_counters_only() {
    let store = Arc::new(Failing {
        inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
        down: AtomicBool::new(true),
    });
    let sink = Arc::new(StoreTailSink::new(store.clone(), ERROR_EDGES_MS));
    let mut w = world(sink.clone());
    drive(&mut w);
    assert_eq!(w.fx.registry.tail_counters(), driven());
    let down = TailSinkError::Store(StoreError::Failed("down".to_owned()));
    assert_eq!(sink.flush(), Err(down));
    assert_eq!(sink.counters().writes_failed, 1);
    assert_eq!(w.fx.registry.tail_counters(), driven());

    store.down.store(false, Ordering::SeqCst);
    sink.flush().unwrap();
    sink.flush().unwrap();
    let back = world(Arc::new(StoreTailSink::new(store, ERROR_EDGES_MS)));
    assert_eq!(back.fx.registry.tail_counters(), driven());
}

/// Asserts that a registry whose sink holds counters bucketed by other edges starts from
/// zero, reports the refusal once, and still counts in memory. Catches: buckets of other
/// ranges read into this build's counters.
#[test]
fn a_store_with_other_edges_starts_the_counters_at_zero() {
    on_both("tail-other-edges", |store| {
        let mut edges = ERROR_EDGES_MS;
        edges[0] -= 1;
        let old = Arc::new(StoreTailSink::new(store.clone(), edges));
        let mut first = world(old.clone());
        drive(&mut first);
        old.flush().unwrap();

        let w = World::of(pool(), &["a"]);
        let sink = Arc::new(StoreTailSink::new(store, ERROR_EDGES_MS));
        let refused = w.fx.registry.set_tail_sink(sink);
        assert!(matches!(refused, Err(TailSinkError::OtherEdges { .. })));
        assert_eq!(w.fx.registry.tail_counters(), TailCounters::default());
    });
}

/// Asserts that a registry given no sink counts exactly as before, in memory only.
/// Catches: counts that wait for a sink being left out of the registry's own counters.
#[test]
fn a_registry_with_no_sink_counts_in_memory() {
    let mut w = World::of(pool(), &["a"]);
    w.fx.registry.set_placer(Arc::new(First));
    w.fx.registry.set_lives(Arc::new(TwoMinutes));
    w.provider = w.fx.registry.provider();
    drive(&mut w);
    assert_eq!(w.fx.registry.tail_counters(), driven());
}
