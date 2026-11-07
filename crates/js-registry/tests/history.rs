//! The record of each lease booking that ends: the registry builds it in its stop and hands
//! it to its log after its lock is released, and the usage hub (the log a server runs)
//! writes it through its sink into the state store at its next write. The registry is driven
//! on a manual clock; nothing sleeps and no test asserts how long anything took.

mod support;

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, HeldRun, HeldState, RefusalReason};

use js_provider::{Candidate, Observed, Pool, StartSpec, StopAck};
use js_registry::{
    DEFAULT_PREPARE_TIMEOUT_MS, DEFAULT_REGAIN_GRACE_MS, ERROR_EDGES_MS, MAX_FINISHED_RUNS,
    MarkedTails, Placer, ProfileLives, Registry, WholeNodeConfig,
};
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_state::sqlite::{Options, SqliteStore};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::{Admission, ClassId, Dims, Request, ResourceClass, Resources};
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

/// A maker of fresh stores of one kind.
type Stores<'a> = &'a dyn Fn() -> Arc<dyn Store>;

/// Runs `test` with fresh in-memory stores and then with fresh SQLite files, one file for
/// each store it asks for.
fn each_store(name: &str, test: impl Fn(Stores<'_>)) {
    test(&fake);
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("js-registry-{name}"));
    let made = AtomicUsize::new(0);
    let sqlite = || -> Arc<dyn Store> {
        let clock: Arc<dyn js_state::Clock> = Arc::new(js_state::ManualClock::default());
        let path = dir
            .path()
            .join(format!("state-{}.db", made.fetch_add(1, Ordering::SeqCst)));
        Arc::new(SqliteStore::open(&path, clock, Options::default()).unwrap())
    };
    test(&sqlite);
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
    each_store("one-row", |store| {
        let mut l = Logged::new(World::new(&["a"], plain), store());
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
fn a_whole_node_request_that_waited_and_gave_a_host_up(store: Stores<'_>) {
    let w = World::new(&["a", "b"], drain_deadline(30_000));
    let mut l = Logged::new(w, store());
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
fn an_estimated_lease_and_one_without_an_estimate(store: Stores<'_>) {
    let w = World::new(&["a", "b"], plain);
    w.fx.registry.set_lives(lives());
    let mut l = Logged::new(w, store());
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
            cpu_mean_milli: 1200,
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
fn a_lost_host_and_a_wall_limit(store: Stores<'_>) {
    let mut l = Logged::new(World::new(&["a", "b"], plain), store());
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
fn requests_cancelled_while_they_waited(store: Stores<'_>) {
    let mut l = Logged::new(World::new(&["a"], plain), store());
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
    let mut l = Logged::new(World::new(&["a"], late), store());
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
fn a_whole_node_lease_without_a_request_id(store: Stores<'_>) {
    let mut l = Logged::new(World::new(&["a"], plain), store());
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
fn leases_put_back_after_a_restart(store: Stores<'_>) {
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
        let mut l = Logged::new(w, store());
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

/// Catches: a whole-node request cancelled after it gave its only marked host up, and so
/// holding no mark when it was cancelled, recorded as one that never held a mark.
fn a_request_cancelled_after_it_gave_its_only_host_up(store: Stores<'_>) {
    let settings = |w: WholeNodeConfig| WholeNodeConfig {
        drain_deadline_ms: Some(20_000),
        reserve_backoff_ms: 600_000,
        ..w
    };
    let mut l = Logged::new(World::new(&["a"], settings), store());
    l.w.share(1, HOUR_S);
    l.w.ask(2, "q");
    assert_eq!(l.w.reserved_host("q"), Some("a".to_string()));
    l.w.step(25_000);
    assert_eq!(l.w.reservation("q"), None);
    l.w.stop(2);
    let row = l.row();
    let outcome = (row.drain_outcome, row.give_up_outcome, row.queued_ms);
    assert_eq!(outcome, (DrainOutcome::MarkDropped, Some(1), Some(25_000)));
}

/// A placer that books the first host it is shown that no whole-node request has set aside.
struct First;

impl Placer for First {
    fn pick(&self, _: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize> {
        open.iter().position(|c| !marked.contains_key(&c.host))
    }
}

/// A class whose leases share a host, each booking what it asks for.
fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// Catches: a lease that shares its host recorded as booking the whole host, or with the
/// host's figures in place of its own share.
fn a_shared_booking_records_its_own_share(store: Stores<'_>) {
    let mut w = World::of(config(4), &["a"]);
    w.fx.registry.set_placer(Arc::new(First));
    w.provider = w.provider.clone().with_classes(vec![whole(), book()]);
    let mut l = Logged::new(w, store());
    let share = Resources::new(1000, 1 << 20, 0, None).unwrap();
    let request = Request {
        class: book().id().clone(),
        resources: share,
        max_wall: Duration::from_secs(600),
        ..request()
    };
    let spec = StartSpec::new(lease(4), request, &[book()]).unwrap();
    assert_eq!(started_on(&l.w.provider.start(spec)), Some("a"));
    l.w.step(5_000);
    l.w.stop(4);
    let end = l.w.now().millis();
    let booked = Booked {
        whole_host: false,
        cpu_milli: 1000,
        memory_bytes: 1 << 20,
    };
    let expected = EndedRow {
        booked: Some(booked),
        ..ordinary(&l, 4, "a", end, 5_000)
    };
    assert_eq!(l.row(), expected);
}

/// Catches: an ordinary lease put back after a restart, with the estimate feed on, recorded
/// without the start the lease file kept, or without its estimate and profile.
fn an_ordinary_lease_put_back_with_the_feed_keeps_its_start_and_estimate(store: Stores<'_>) {
    let w = World::of(config(4), &["a"]);
    w.fx.registry.set_lives(lives());
    let mut l = Logged::new(w, store());
    let start = l.w.now().millis();
    let booking = booking_at("a", 7, None, start, start + 600_000);
    assert_eq!(l.w.provider.seat_booked(&booking, None), Ok(()));
    let key = ProfileKey::parse("two").unwrap();
    let registry = &l.w.fx.registry;
    let queued = registry.queue_keyed_run(&host("a"), &lease(7), job("r7"), Some(key.clone()));
    queued.unwrap();
    l.w.step(150_000);
    l.w.stop(7);
    let end = l.w.now().millis();
    let expected = EndedRow {
        profile: Some(key),
        estimated_end: Some(start + 120_000),
        error_ms: Some(30_000),
        ..ordinary(&l, 7, "a", end, 150_000)
    };
    assert_eq!(l.row(), expected);
}

/// One case per field of the record (design section 10.1), each over the in-memory store and
/// over SQLite: the queue wait and the drain and give-up outcomes of a whole-node request
/// (one that waited and gave a host up, one cancelled while it waited with a mark, without
/// one, and after it gave its only host up, one with no request id) and of an ordinary
/// lease, the estimate against the actual end, the host and the lease, why it ended, what
/// was booked (the whole host, or a share) and what was used, and leases put back after a
/// restart (with and without the estimate feed). The stored form's own round trip, and the
/// skip of a row of a later version, are js-usage's tests.
#[test]
fn history_row_fields_match_section_10_1() {
    each_store("fields", |store| {
        a_whole_node_request_that_waited_and_gave_a_host_up(store);
        an_estimated_lease_and_one_without_an_estimate(store);
        a_lost_host_and_a_wall_limit(store);
        requests_cancelled_while_they_waited(store);
        a_request_cancelled_after_it_gave_its_only_host_up(store);
        a_whole_node_lease_without_a_request_id(store);
        a_shared_booking_records_its_own_share(store);
        leases_put_back_after_a_restart(store);
        an_ordinary_lease_put_back_with_the_feed_keeps_its_start_and_estimate(store);
    });
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

/// Catches: a whole-node prepare that fails (refused by the agent, or not ready within
/// `prepare_timeout`) freeing its booking with no record, a record not marked
/// `failed-to-start` or without the booking's figures, and the later stop of that lease
/// recording it again.
#[test]
fn a_failed_prepare_records_one_failed_to_start_row() {
    each_store("failed-prepare", |store| {
        let mut l = Logged::new(World::new(&["a"], plain), store());
        l.w.ask(2, "q");
        assert_eq!(l.w.lease_on("a"), Some(lease(2)));
        l.w.asks.clear();
        l.w.step(10_000);
        let mut refused = l.w.fx.heartbeat("a", l.w.epochs["a"]);
        refused.exclusive_refused = Some(ExclusiveRefusal {
            lease_id: lease(2).to_string(),
            code: RefusalReason::NotQuiet,
            detail: String::new(),
        });
        l.w.fx.registry.heartbeat(&refused, &cred("t")).unwrap();
        assert_eq!(l.w.lease_on("a"), None);
        let failed = EndedRow {
            ended_reason: EndedReason::FailedToStart,
            drain_outcome: DrainOutcome::MarkHeld,
            give_up_outcome: Some(0),
            ..ordinary(&l, 2, "a", l.w.now().millis(), 10_000)
        };
        assert_eq!(l.row(), failed);
        l.w.stop(2);
        assert_eq!(l.hub.write_history(), None);

        let mut l = Logged::new(World::new(&["a"], plain), store());
        l.w.ask(3, "r");
        l.w.asks.clear();
        l.w.step(DEFAULT_PREPARE_TIMEOUT_MS);
        assert!(l.rows().is_empty());
        l.w.fx.registry.advance();
        let row = l.row();
        let ended = (row.ended_reason, row.run_ms);
        assert_eq!(
            ended,
            (EndedReason::FailedToStart, Some(DEFAULT_PREPARE_TIMEOUT_MS))
        );
    });
}

/// A source of lives whose answer changes at every read: two minutes, then three, then
/// two again.
struct Flipping(AtomicU32);

impl ProfileLives for Flipping {
    fn wall_minutes_p90(&self, _: &ProfileKey) -> Option<u32> {
        Some(2 + self.0.fetch_add(1, Ordering::SeqCst) % 2)
    }
}

/// Catches: the record and the tail counters reading the estimate of one stopped lease
/// separately, so that a source that changes between the reads gives them errors that
/// disagree (one read gives 30 s late, the next 30 s early).
#[test]
fn the_record_and_the_tail_counters_read_one_estimate() {
    let w = World::new(&["a"], plain);
    w.fx.registry
        .set_lives(Arc::new(Flipping(AtomicU32::new(0))));
    let mut l = Logged::new(w, fake());
    l.w.share(1, 900);
    let key = ProfileKey::parse("k").unwrap();
    let registry = &l.w.fx.registry;
    registry
        .queue_keyed_run(&host("a"), &lease(1), job("r1"), Some(key))
        .unwrap();
    l.w.step(150_000);
    l.w.stop(1);
    let error = l.row().error_ms.unwrap();
    let counters = l.w.fx.registry.tail_counters();
    assert_eq!(counters.error_sum_ms, error);
    let bucket = ERROR_EDGES_MS.partition_point(|edge| *edge < error);
    assert_eq!(counters.error_buckets[bucket], 1);
}

/// Catches: a lease recorded once, put back from the lease file after a restart (as when
/// the save of its release failed) and stopped again, recorded a second time under its
/// later end.
#[test]
fn a_lease_put_back_and_stopped_again_is_recorded_once() {
    each_store("stopped-again", |store| {
        let shared = store();
        let mut first = Logged::new(World::new(&["a"], plain), shared.clone());
        first.w.share(1, 900);
        first.w.step(5_000);
        first.w.stop(1);
        let recorded = first.row();
        let mut second = Logged::new(World::new(&["a"], plain), shared);
        second.w.step(60_000);
        let now = second.w.now().millis();
        let booking = booking_at("a", 1, None, now, now + 900_000);
        assert_eq!(second.w.provider.seat_booked(&booking, None), Ok(()));
        second.w.stop(1);
        assert_eq!(second.rows(), [recorded]);
    });
}

/// Queues run `name` of lease `1-1` on `a`, named by `key`, and runs it to its end.
fn run_through(l: &Logged, name: &str, key: Option<&str>) {
    let key = key.map(|key| ProfileKey::parse(key).unwrap());
    let registry = &l.w.fx.registry;
    registry
        .queue_keyed_run(&host("a"), &lease(1), job(name), key)
        .unwrap();
    let epoch = l.w.epochs["a"];
    let mut running = l.w.fx.heartbeat("a", epoch);
    running.held = vec![HeldRun {
        run_id: name.into(),
        state: HeldState::Running,
    }];
    registry.heartbeat(&running, &cred("t")).unwrap();
    let done = l.w.fx.heartbeat("a", epoch);
    registry.heartbeat(&done, &cred("t")).unwrap();
}

/// Catches: the record of a lease taking its profile only from the finished runs the
/// registry still keeps, so that a long-lived lease whose named run was dropped past
/// [`MAX_FINISHED_RUNS`] loses it; and a lease whose runs named two profiles recorded with
/// any but the first.
#[test]
fn the_profile_of_a_lease_outlives_its_runs() {
    let mut l = Logged::new(World::new(&["a"], plain), fake());
    l.w.share(1, HOUR_S);
    run_through(&l, "named", Some("first"));
    run_through(&l, "renamed", Some("second"));
    for n in 0..MAX_FINISHED_RUNS {
        run_through(&l, &format!("r{n}"), None);
    }
    let kept = l.w.fx.registry.runs_of(&lease(1));
    assert!(kept.iter().all(|view| view.run_id != "named"));
    l.w.stop(1);
    assert_eq!(l.row().profile, Some(ProfileKey::parse("first").unwrap()));
}

/// Catches: the stop of a lease whose waiting request the wait list already dropped (not
/// presented within the regain grace) recorded as a cancellation, and a request put back
/// from the ledger and cancelled before its consumer presented it again recorded with no
/// wait, or not at all.
///
/// Runs under both placements: under `--placement js-sched` the engine's wait list says
/// how the request waited.
fn waiting_requests_dropped_or_put_back_before_the_stop(p: Placement) {
    let mut l = Logged::new(World::new_in(p, &["a"], plain), fake());
    l.w.share(1, HOUR_S);
    l.w.ask(2, "q");
    l.w.asks.clear();
    l.w.step(DEFAULT_REGAIN_GRACE_MS + POLL_MS);
    assert_eq!(l.w.fx.registry.queue_position(&id("q")), None);
    l.w.stop(2);
    assert_eq!(l.hub.write_history(), None);

    let mut first = World::new_in(p, &["a"], plain);
    first.share(1, HOUR_S);
    let asked = first.now().millis();
    first.ask(2, "q");
    let ledger = first.fx.registry.reservation_ledger();
    let mut l = Logged::new(World::new_in(p, &["a"], plain), fake());
    l.w.share(1, HOUR_S);
    l.w.step(10_000);
    assert_eq!(l.w.fx.registry.restore_reservations(&ledger).resumed, 1);
    assert_eq!(l.w.fx.registry.bind_waiting(&[(lease(2), id("q"))]), 1);
    l.w.stop(2);
    let row = l.row();
    let place = (row.host, row.run_ms, row.ended_reason);
    assert_eq!(place, (None, Some(0), EndedReason::CancelledWhileWaiting));
    assert_eq!(row.queued_ms, Some(l.w.now().millis() - asked));
    let outcome = (row.drain_outcome, row.give_up_outcome);
    assert_eq!(outcome, (DrainOutcome::MarkDropped, Some(0)));
}

crate::both!(waiting_requests_dropped_or_put_back_before_the_stop);
