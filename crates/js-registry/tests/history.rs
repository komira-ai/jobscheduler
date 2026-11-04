//! The record of each lease booking that ends: the registry builds it in its stop and hands
//! it to its log after its lock is released, and the usage hub (the log a server runs)
//! writes it through its sink into the state store at its next write. The registry is driven
//! on a manual clock; nothing sleeps and no test asserts how long anything took.

mod support;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_provider::{Observed, Pool, StopAck};
use js_registry::{ERROR_EDGES_MS, ProfileLives, Registry, WholeNodeConfig};
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_usage::{
    AllocationLog, Beat, BeatLoad, Booked, DrainOutcome, EndedHistory, EndedReason, EndedRow,
    KeyProfile, Limits, LoadIn, MinuteIn, NoKeys, NodeMinute, Observation, Observed as Taken,
    ProfileKey, RunEnd, SinkCounters, UsageFault, UsageHub, UsageSink, Used,
};
use support::world::*;
use support::*;

/// A sink that keeps the records of ended allocations in a state store, refusing them while
/// `refuse` is set, and takes every reading's minutes as fresh. It keeps nothing else.
struct History {
    store: Arc<dyn Store>,
    refuse: AtomicBool,
}

impl UsageSink for History {
    fn observe(&self, _: &HostId, _: u64, load: &LoadIn, _: Limits, _: u64) -> Observation {
        Observation {
            observed: Taken::Accepted,
            fresh: load.minutes.clone(),
        }
    }

    fn run_ended(&self, _: &RunEnd, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        Ok(Vec::new())
    }

    fn prune(&self, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn node_rows(
        &self,
        _: &HostId,
        _: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageFault> {
        Ok(Vec::new())
    }

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        Ok(None)
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters::default()
    }

    fn allocation_ended(&self, row: &EndedRow) -> Result<(), UsageFault> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(UsageFault("refused".to_string()));
        }
        let put = EndedHistory::new(&*self.store).put(row);
        put.map(drop).map_err(|err| UsageFault(err.to_string()))
    }
}

/// A world whose registry logs into a usage hub over a [`History`] sink on `store`.
struct Logged {
    w: World,
    hub: Arc<UsageHub>,
    sink: Arc<History>,
}

impl Logged {
    fn new(w: World, store: Arc<dyn Store>) -> Self {
        let logged = Self::unset(w, store);
        logged.w.fx.registry.set_allocation_log(logged.hub.clone());
        logged
    }

    /// The world, hub and sink, with no log set on the registry.
    fn unset(w: World, store: Arc<dyn Store>) -> Self {
        let sink = Arc::new(History {
            store,
            refuse: AtomicBool::new(false),
        });
        let limits = Limits {
            threads: 4,
            memory_bytes: 8 << 30,
        };
        let hub = Arc::new(UsageHub::new(sink.clone(), Arc::new(NoKeys), limits, 0));
        Self { w, hub, sink }
    }

    /// Writes what the hub has queued, and reads back every stored record, oldest first.
    fn rows(&self) -> Vec<EndedRow> {
        self.hub.write_history();
        let limit = NonZeroUsize::new(1000).unwrap();
        let history = EndedHistory::new(&*self.sink.store);
        history.range(0, u64::MAX, limit).unwrap().rows
    }

    /// The only stored record.
    fn row(&self) -> EndedRow {
        let rows = self.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows[0].clone()
    }

    /// A beat of `on` carrying `(minute, mean cores, working set)` minutes of lease
    /// `sequence`.
    fn measure(&self, on: &str, sequence: u64, minutes: &[(u16, u32, u64)]) {
        let minute = |(minute, mean, ws): &(u16, u32, u64)| MinuteIn {
            lease: lease(sequence),
            minute: *minute,
            mean_cores_milli: *mean,
            peak_window_milli: *mean,
            working_set_max_bytes: *ws,
        };
        let load = LoadIn {
            seq: 1,
            mono_usec: 1,
            cpu_usage_usec: 1,
            working_set_bytes: 1,
            actions_cpu_limited: false,
            covered: Vec::new(),
            minutes: minutes.iter().map(minute).collect(),
            clocks: Vec::new(),
        };
        let on = host(on);
        let beat = Beat {
            host: &on,
            epoch: 1,
            load: BeatLoad::Read {
                load,
                records_dropped: 0,
            },
            results: &[],
        };
        self.hub.on_beat(beat, 0, &|_, _| true);
    }

    /// What the pool books for host `on`.
    fn booked_as(&self, on: &str) -> Booked {
        let views = self.w.fx.registry.hosts();
        let view = views.iter().find(|view| view.id == host(on)).unwrap();
        Booked {
            whole_host: true,
            cpu_milli: view.shape.resources().cpu_millis(),
            memory_bytes: view.shape.resources().memory_bytes(),
        }
    }
}

/// A fresh in-memory store.
fn fake() -> Arc<dyn Store> {
    let clock = Arc::new(js_state::ManualClock::default());
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

/// Runs `test` over a fresh in-memory store and then over a fresh SQLite file.
fn on_both(name: &str, test: impl Fn(Arc<dyn Store>)) {
    test(fake());
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-registry-{name}"));
    let clock: Arc<dyn js_state::Clock> = Arc::new(js_state::ManualClock::default());
    let path = dir.path().join("state.db");
    let sqlite = SqliteStore::open(&path, clock, Options::default()).unwrap();
    test(Arc::new(sqlite));
}

/// A fixed table of P90 minutes by profile name.
struct Table(BTreeMap<&'static str, u32>);

impl ProfileLives for Table {
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
        self.0.get(key.as_str()).copied()
    }
}

fn lives() -> Arc<Table> {
    Arc::new(Table(BTreeMap::from([("two", 2)])))
}

/// The row of an ordinary lease `sequence` on `on`, ended at `end` after `run_ms`, as the
/// fields every case shares: no profile, queued for nothing, no estimate, nothing used.
fn ordinary(l: &Logged, sequence: u64, on: &str, end: u64, run_ms: u64) -> EndedRow {
    EndedRow {
        id: js_usage::AllocationId::of_lease(&lease(sequence)),
        profile: None,
        host: Some(host(on)),
        booked: Some(l.booked_as(on)),
        queued_ms: Some(0),
        run_ms: Some(run_ms),
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

/// Catches: no record written when a lease's booking ends, one written under another id,
/// and a second stop of the same lease (the lease table repeats a stop it got no answer
/// to) writing a second record. Over the in-memory store and over SQLite.
#[test]
fn an_ended_lease_booking_writes_one_history_row() {
    on_both("one-row", |store| {
        let mut l = Logged::new(World::new(&["a"], plain), store);
        assert_eq!(started_on(&l.w.share(1, 900)), Some("a"));
        l.w.step(60_000);
        l.w.stop(1);
        let end = l.w.now().millis();
        assert_eq!(l.row(), ordinary(&l, 1, "a", end, 60_000));
        l.w.step(5_000);
        l.w.stop(1);
        assert_eq!(l.hub.write_history(), None);
        assert_eq!(l.rows().len(), 1);
    });
}

/// Catches, for a whole-node request that waited 40 s, gave one host up and was then served
/// on the host its second mark set aside: a queue wait counted from anything but its first
/// presentation to its start, the give-up lost, the mark's outcome not set, a whole host not
/// recorded as booked whole, and the run counted from the presentation.
fn a_whole_node_request_that_waited_and_gave_a_host_up() {
    let w = World::new(&["a", "b"], drain_deadline(30_000));
    let mut l = Logged::new(w, fake());
    l.w.share(1, HOUR_S);
    l.w.share(2, HOUR_S);
    l.w.ask(3, "q");
    assert_eq!(l.w.reserved_host("q"), Some("a".to_string()));
    l.w.step(30_000);
    assert_eq!(l.w.reserved_host("q"), Some("b".to_string()));
    l.w.step(5_000);
    l.w.stop(2);
    l.w.step(5_000);
    assert_eq!(l.w.lease_on("b"), Some(lease(3)));
    l.w.step(10_000);
    l.w.stop(3);
    let rows = l.rows();
    let id = js_usage::AllocationId::of_lease(&lease(3));
    let row = rows.iter().find(|row| row.id == id).unwrap();
    let expected = EndedRow {
        queued_ms: Some(40_000),
        drain_outcome: DrainOutcome::MarkHeld,
        give_up_outcome: Some(1),
        ..ordinary(&l, 3, "b", l.w.now().millis(), 10_000)
    };
    assert_eq!(*row, expected);
}

/// Catches: an estimate, its end or its error that disagree with what the tail counters
/// counted for the same lease, the profile of the lease's keyed run left out, what the hub
/// measured of the lease left out or not the highest minute, and a lease with no keyed run
/// given an estimate.
fn an_estimated_lease_and_one_without_an_estimate() {
    let w = World::new(&["a", "b"], plain);
    w.fx.registry.set_lives(lives());
    let mut l = Logged::new(w, fake());
    let start = l.w.now().millis();
    l.w.share(1, 900);
    l.w.share(2, 900);
    let key = ProfileKey::parse("two").unwrap();
    let registry = &l.w.fx.registry;
    let queued = registry.queue_keyed_run(&host("a"), &lease(1), job("r1"), Some(key.clone()));
    queued.unwrap();
    l.measure("a", 1, &[(0, 1500, 1 << 30), (1, 900, 2 << 30)]);
    l.w.step(150_000);
    l.w.stop(1);
    l.w.stop(2);
    let end = l.w.now().millis();
    let error: i64 = 30_000;
    let estimated = EndedRow {
        profile: Some(key),
        estimated_end: Some(start + 120_000),
        error_ms: Some(error),
        used: Some(Used {
            cpu_milli: 1500,
            memory_bytes: 2 << 30,
        }),
        ..ordinary(&l, 1, "a", end, 150_000)
    };
    assert_eq!(l.rows(), [estimated, ordinary(&l, 2, "b", end, 150_000)]);
    let bucket = ERROR_EDGES_MS.partition_point(|edge| *edge < error);
    let counters = l.w.fx.registry.tail_counters();
    assert_eq!(counters.error_buckets[bucket], 1);
    assert_eq!(counters.error_sum_ms, error);
}

/// Catches: the stop of a lease whose host was lost, and of one that reached the end its
/// wall limit allows, recorded as released.
fn a_lost_host_and_a_wall_limit() {
    let mut l = Logged::new(World::new(&["a", "b"], plain), fake());
    l.w.share(1, 900);
    l.w.share(2, 60);
    l.w.silent.insert("a".to_string());
    l.w.step(60_000);
    l.w.stop(1);
    l.w.stop(2);
    let reasons: Vec<EndedReason> = l.rows().iter().map(|row| row.ended_reason).collect();
    assert_eq!(
        reasons,
        [EndedReason::Lost, EndedReason::StoppedAtWallLimit]
    );
}

/// Catches: a whole-node request cancelled (its lease stopped) while it waited written with
/// a host or a run time, its wait not counted, its give-ups not counted, and the mark it
/// held (or did not) misreported.
fn requests_cancelled_while_they_waited() {
    let mut l = Logged::new(World::new(&["a"], plain), fake());
    l.w.share(1, HOUR_S);
    l.w.ask(2, "q");
    assert_eq!(l.w.reserved_host("q"), Some("a".to_string()));
    l.w.step(10_000);
    l.w.stop(2);
    let end = l.w.now().millis();
    let cancelled = EndedRow {
        id: js_usage::AllocationId::of_lease(&lease(2)),
        profile: None,
        host: None,
        booked: None,
        queued_ms: Some(10_000),
        run_ms: Some(0),
        ended_reason: EndedReason::CancelledWhileWaiting,
        drain_outcome: DrainOutcome::MarkDropped,
        give_up_outcome: Some(0),
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    };
    assert_eq!(l.row(), cancelled);

    let late = |w: WholeNodeConfig| WholeNodeConfig {
        drain_after_ms: 600_000,
        ..w
    };
    let mut l = Logged::new(World::new(&["a"], late), fake());
    l.w.share(1, HOUR_S);
    l.w.ask(2, "q");
    assert_eq!(l.w.reservation("q"), None);
    l.w.stop(2);
    let row = l.row();
    assert_eq!(
        (row.drain_outcome, row.queued_ms),
        (DrainOutcome::NoMark, Some(0))
    );
}

/// Catches: a whole-node lease whose start carried no request id (it never waits and holds
/// no mark) recorded as an ordinary lease or as one whose mark held.
fn a_whole_node_lease_without_a_request_id() {
    let mut l = Logged::new(World::new(&["a"], plain), fake());
    l.w.provider.start(World::whole_spec(5, None, 600));
    assert_eq!(l.w.lease_on("a"), Some(lease(5)));
    l.w.stop(5);
    let row = l.row();
    let queue = (row.queued_ms, row.drain_outcome, row.give_up_outcome);
    assert_eq!(queue, (Some(0), DrainOutcome::NoMark, Some(0)));
}

/// Catches: a lease put back after a restart given a queue it never had (a whole-node
/// request's wait is not kept) or a run time without a known start, and one put back with
/// its start (the estimate feed on) not given its run time.
fn leases_put_back_after_a_restart() {
    let now = 2_000_000;
    let cases: [(World, Option<u64>, DrainOutcome, Option<u64>); 3] = [
        (
            World::of(config(4), &["a"]),
            Some(0),
            DrainOutcome::None,
            None,
        ),
        (World::new(&["a"], plain), None, DrainOutcome::Unknown, None),
        (
            {
                let w = World::new(&["a"], plain);
                w.fx.registry.set_lives(lives());
                w
            },
            None,
            DrainOutcome::Unknown,
            Some(now - 1_000_000),
        ),
    ];
    for (w, queued, drain, run_ms) in cases {
        let mut l = Logged::new(w, fake());
        let booking = booking_at("a", 7, None, 1_000_000, now + 600_000);
        assert_eq!(l.w.provider.seat_booked(&booking, None), Ok(()));
        l.w.fx
            .clock
            .advance(std::time::Duration::from_millis(1_000_000));
        l.w.stop(7);
        let row = l.row();
        assert_eq!((row.queued_ms, row.drain_outcome), (queued, drain));
        assert_eq!((row.run_ms, row.give_up_outcome), (run_ms, None));
    }
}

/// One case per field of the record (design section 10.1): the queue wait and the drain and
/// give-up outcomes of a whole-node request and of an ordinary lease, the estimate against
/// the actual end, the host and the lease, a request cancelled before it had a host, what
/// was booked and what was used, and leases put back after a restart. The stored form's own
/// round trip, and the skip of a row of a later version, are js-usage's tests.
#[test]
fn history_row_fields_match_section_10_1() {
    a_whole_node_request_that_waited_and_gave_a_host_up();
    an_estimated_lease_and_one_without_an_estimate();
    a_lost_host_and_a_wall_limit();
    requests_cancelled_while_they_waited();
    a_whole_node_lease_without_a_request_id();
    leases_put_back_after_a_restart();
}

/// A log that asks the registry something before it queues the row: were the registry's
/// lock held across the call to the log, the call would wait on it for ever.
struct Reentrant {
    registry: Registry,
    hub: Arc<UsageHub>,
}

impl AllocationLog for Reentrant {
    fn ended(&self, row: EndedRow) {
        let _ = self.registry.tail_counters();
        self.hub.ended(row);
    }
}

/// Catches: a store that refuses the write failing the stop (or changing its answer), the
/// failure not counted, and the registry's lock held across the call to the log.
#[test]
fn a_failed_history_write_does_not_fail_the_stop() {
    let mut l = Logged::unset(World::new(&["a"], plain), fake());
    // With no log set, a stop records nothing.
    l.w.share(9, 900);
    l.w.stop(9);
    assert_eq!(l.hub.write_history(), None);
    let registry = l.w.fx.registry.clone();
    let hub = l.hub.clone();
    registry.set_allocation_log(Arc::new(Reentrant {
        registry: registry.clone(),
        hub,
    }));
    l.sink.refuse.store(true, Ordering::SeqCst);
    l.w.share(1, 900);
    let stopped = l.w.provider.stop(&lease(1));
    let read_back = Observed::Absent;
    assert_eq!(stopped, StopAck::Stopped { read_back });
    assert_eq!(l.w.lease_on("a"), None);
    let refused = Some(Err(UsageFault("refused".to_string())));
    assert_eq!(l.hub.write_history(), refused);
    assert_eq!(l.hub.counters().history_failed, 1);
    l.sink.refuse.store(false, Ordering::SeqCst);
    assert_eq!(l.rows().len(), 1);
    assert_eq!(l.hub.counters().history_written, 1);
}
