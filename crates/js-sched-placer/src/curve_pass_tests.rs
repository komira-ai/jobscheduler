//! The curve mode through whole passes over the job tables on the in-memory fake: what a pass
//! reads of the live jobs, what it commits to the store, and what it tells the server's metrics.
//! The store's clock and the placer's are the test's, both at [`NOW_MS`] when a pass runs.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_jobs::{
    AllocationId, AllocationRecord, AllocationStep, Booked, Capacity, JobId, JobState, NewJob,
};
use js_registry::Placer;
use js_types::{ManualClock, Resources, Shape, Timestamp};

use crate::curves::{CurveHosts, CurveSettings};
use crate::engine::Settings;
use crate::engine::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics};
use crate::engine::rig::{Tables, at, host};
use crate::place::SchedPlacer;
use crate::sizes::Curves;
use crate::sizes::curve_rig::{
    booking, burst_at, busy_until, capture_targets, queued_of, spec_of_class, teach,
};

/// The farm time of the passes, in milliseconds.
const NOW_MS: u64 = 5_000_000;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// What the passes told the metrics: how many wait bounds ran out.
#[derive(Default)]
struct Counting {
    expired: Cell<u64>,
}

impl PassMetrics for Counting {
    fn job_placed(&self, _: Duration) {}
    fn store_write_failed(&self) {}
    fn curve_wait_expired(&self) {
        self.expired.set(self.expired.get() + 1);
    }
}

impl JobStarts for Counting {
    fn placed(&self, _: &JobId, _: &js_types::HostId) {}
    fn tainted(&self, _: &js_types::HostId, _: &JobId) {}
}

/// A placer in mode `curves` that books by curve as `curve` says, over the host `a` of 88 cores,
/// and its clock, at [`NOW_MS`].
fn placer_in(curves: Curves, curve: CurveSettings) -> (SchedPlacer, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(NOW_MS));
    let settings = Settings {
        curves,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock.clone()).with_curve_settings(curve);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(88_000, 256 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, at(NOW_MS));
    (placer, clock)
}

/// Records the job `id` of `class`, which leaves its CPU to its class's history, booked `cpu`
/// millicores and 64 MiB, placed on `a`, and starts it at the store's time now when `started`
/// says so (else it is only starting): its allocation.
fn running(
    tables: &Tables,
    (id, class): (&str, &str),
    (cpu, started): (u64, bool),
) -> AllocationRecord {
    running_named(tables, (id, class), (0, cpu), started)
}

/// [`running`], the job naming `named` millicores of CPU in its needs (0: none, sized by the
/// history) and booked `cpu`.
fn running_named(
    tables: &Tables,
    (id, class): (&str, &str),
    (named, cpu): (u64, u64),
    started: bool,
) -> AllocationRecord {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec_of_class(class, named, 64 * MIB),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap();
    let (_, row) = tables
        .jobs
        .place(&id, host("a"), booking(cpu, 64 * MIB))
        .unwrap();
    tables
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    if started {
        tables
            .jobs
            .transition(&id, JobState::Starting, JobState::Running)
            .unwrap();
    }
    row
}

/// A sink that ignores whatever a pass tells it, wait bounds included.
struct Silent;

impl PassMetrics for Silent {
    fn job_placed(&self, _: Duration) {}
    fn store_write_failed(&self) {}
}

impl JobStarts for Silent {
    fn placed(&self, _: &JobId, _: &js_types::HostId) {}
    fn tainted(&self, _: &js_types::HostId, _: &JobId) {}
}

/// One pass of `placer` over `tables` with `a` open as `offer` says: the jobs it placed.
fn pass(placer: &SchedPlacer, tables: &Tables, offer: OpenHost, told: &Counting) -> Vec<JobId> {
    pass_with(placer, tables, offer, (told, told))
}

/// [`pass`], telling `sinks`.
fn pass_with(
    placer: &SchedPlacer,
    tables: &Tables,
    offer: OpenHost,
    sinks: (&dyn JobStarts, &dyn PassMetrics),
) -> Vec<JobId> {
    placer.heard(&host("a"), None);
    let open = BTreeMap::from([(host("a"), offer)]);
    let none = BTreeSet::new();
    let hosts = JobHosts {
        open: &open,
        hello: &none,
        tainted: &none,
        whole: None,
    };
    placer.place_jobs_with(&tables.jobs, &hosts, sinks).unwrap()
}

/// A host that holds its bookings to `cpu_millis` of CPU, as the store does for a host that
/// reports load admission, and to the memory and disk of its hello.
fn offering(cpu_millis: u64) -> OpenHost {
    let capacity = Capacity {
        cpu_millis,
        memory_bytes: 256 * GIB,
        disk_bytes: 0,
        accelerators: 0,
    };
    OpenHost {
        capacity: Some(capacity),
        ..OpenHost::default()
    }
}

/// The curve settings that name the hosts `named` and wait `max_wait_ms`.
fn naming(named: &[&str], max_wait_ms: u64) -> CurveSettings {
    CurveSettings {
        hosts: CurveHosts::new(named.iter().map(|name| host(name)).collect()),
        max_wait: Duration::from_millis(max_wait_ms),
    }
}

/// Asserts the commit of a placement on a curve host end to end: 32 running jobs of `steady`,
/// each of 3000 m and 110 s old, fill a host whose store offers 96 800 m of CPU to 96 000 m; a
/// job of `steady` whose curve fits every slot is placed on it, committed with its 3000 m, and
/// the store's allocation shows 99 000 m booked against the 96 800 m it offers (its CPU not held
/// to the store's figure); the same pass over a host the settings do not name leaves the job
/// queued. Catches a commit still held to the store's CPU, and a lift on a host not named.
#[test]
fn a_pass_commits_a_job_past_the_stores_cpu_on_a_curve_host_and_not_on_another() {
    for (named, placed) in [("a", true), ("z", false)] {
        let tables = Tables::new();
        let (placer, _) = placer_in(Curves::On, naming(&[named], 30_000));
        teach(
            placer.engine().sizes(),
            &tables,
            ("steady", 24),
            &busy_until(100, 64, 140),
        );
        tables.set_ms(NOW_MS - 110_000);
        for at in 0..32 {
            let id = format!("old-{at:02}");
            running(&tables, (&id, "steady"), (3_000, true));
        }
        tables.set_ms(NOW_MS);
        queued_of(&tables, "next", "steady");
        let told = Counting::default();
        let done = pass(&placer, &tables, offering(96_800), &told);
        assert_eq!(
            done,
            if placed {
                vec![JobId::parse("next").unwrap()]
            } else {
                Vec::new()
            },
            "{named}"
        );
        let record = tables
            .jobs
            .get_job(&JobId::parse("next").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            record.state,
            if placed {
                JobState::Placed
            } else {
                JobState::Queued
            }
        );
        if placed {
            let booked = tables
                .jobs
                .get_allocation(&record.id)
                .unwrap()
                .unwrap()
                .booked;
            assert!(
                matches!(
                    booked,
                    Booked::Shape {
                        cpu_millis: 3_000,
                        ..
                    }
                ),
                "{booked:?}"
            );
        }
    }
}

/// A host `a` whose three running jobs of 30 000 m (90 000 m) leave a job of `early` (1000 m
/// flat) room by its flat number only (its burst of 22 000 m does not fit), with a wait bound of
/// 0, and that job queued as `next`.
fn crowded() -> (SchedPlacer, Tables) {
    let tables = Tables::new();
    let (placer, _) = placer_in(Curves::On, naming(&[], 0));
    let early = burst_at(20, 352, 140);
    teach(placer.engine().sizes(), &tables, ("early", 24), &early);
    for at in 0..3 {
        running(&tables, (&format!("fill-{at}"), "fill"), (30_000, true));
    }
    queued_of(&tables, "next", "early");
    (placer, tables)
}

/// Asserts the wait bound as the server's metrics see it: with a wait bound of 0 the pass over a
/// crowded host places the job of `early` at once and tells the metrics one wait bound ran out,
/// and a pass after it with nothing queued tells none; a sink that ignores wait bounds is placed
/// for the same. Catches an expiry that is not told, or told again by a later pass.
#[test]
fn a_pass_tells_the_metrics_of_each_wait_bound_that_ran_out_once() {
    let (placer, tables) = crowded();
    let told = Counting::default();
    let done = pass(&placer, &tables, offering(96_800), &told);
    assert_eq!(done, vec![JobId::parse("next").unwrap()]);
    assert_eq!(told.expired.get(), 1);
    assert!(pass(&placer, &tables, offering(96_800), &told).is_empty());
    assert_eq!(told.expired.get(), 1);

    let (placer, tables) = crowded();
    let done = pass_with(&placer, &tables, offering(96_800), (&Silent, &Silent));
    assert_eq!(done, vec![JobId::parse("next").unwrap()]);
}

/// Asserts what a pass reads of the live jobs: a job of `early` that started 30 s ago (past its
/// burst) is charged a quarter core at every slot and one that is allocated and not started is
/// charged as at age 0 (its burst of 22 000 m at the 10th slot), so the host's fullest booked
/// slot is 22 250 m; and once that job has started and 30 s have passed, a pass reads its start
/// and both are past their burst (500 m). Catches live jobs all aged 0, all aged by the start of
/// another, and a start never read.
#[test]
fn a_pass_charges_each_live_job_at_the_age_its_record_shows() {
    let tables = Tables::new();
    let (placer, clock) = placer_in(Curves::On, CurveSettings::default());
    teach(
        placer.engine().sizes(),
        &tables,
        ("early", 24),
        &burst_at(20, 352, 140),
    );
    tables.set_ms(NOW_MS - 30_000);
    running(&tables, ("past", "early"), (1_000, true));
    tables.set_ms(NOW_MS);
    running(&tables, ("fresh", "early"), (1_000, false));
    let told = Counting::default();
    assert!(pass(&placer, &tables, offering(96_800), &told).is_empty());
    let peaks = placer.curve_view().unwrap().host_peaks;
    assert_eq!(peaks, BTreeMap::from([(host("a"), 22_250)]));

    let fresh = JobId::parse("fresh").unwrap();
    tables
        .jobs
        .transition(&fresh, JobState::Starting, JobState::Running)
        .unwrap();
    tables.set_ms(NOW_MS + 30_000);
    clock.set(Timestamp::from_millis(NOW_MS + 30_000));
    pass(&placer, &tables, offering(96_800), &told);
    let peaks = placer.curve_view().unwrap().host_peaks;
    assert_eq!(
        peaks,
        BTreeMap::from([(host("a"), 500)]),
        "both past their burst"
    );
}

/// Asserts that a running job is charged by its class's curve only when it left its CPU to the
/// history, as it was booked: a job of `early` that named its 1000 m (and was booked flat) is
/// charged 1000 m at every slot, so the host's fullest slot is 1000 m; the same job leaving its
/// CPU at 0 is charged its class's burst at age 0 (22 000 m).
/// Catches a running job that named its CPU charged by its class's curve, which would let the
/// host take jobs onto the cores the named job uses.
#[test]
fn a_running_job_that_named_its_cpu_is_charged_its_flat_number_at_every_slot() {
    for (named, peak) in [(1_000, 1_000), (0, 22_000)] {
        let tables = Tables::new();
        let (placer, _) = placer_in(Curves::On, CurveSettings::default());
        teach(
            placer.engine().sizes(),
            &tables,
            ("early", 24),
            &burst_at(20, 352, 140),
        );
        tables.set_ms(NOW_MS);
        running_named(&tables, ("held", "early"), (named, 1_000), true);
        assert!(pass(&placer, &tables, offering(96_800), &Counting::default()).is_empty());
        let peaks = placer.curve_view().unwrap().host_peaks;
        assert_eq!(peaks, BTreeMap::from([(host("a"), peak)]), "named {named}");
    }
}

/// Asserts how the engine follows the live allocations of the job table under the curve mode: a
/// share is taken up and read until its start is known (the started job once, the one only
/// starting at every pass); a whole-host booking and a freed allocation are no live job; a job
/// placed again (a new allocation) is taken up again and read again; and one the table no longer
/// holds live is dropped. Catches a start read at every pass, a whole host or a freed
/// allocation charged as a share, and a stale allocation kept.
#[test]
fn the_live_jobs_follow_the_live_allocations_and_are_read_until_they_start() {
    let tables = Tables::new();
    let (placer, _) = placer_in(Curves::On, CurveSettings::default());
    tables.set_ms(NOW_MS - 30_000);
    let started = running(&tables, ("started", "early"), (1_000, true));
    let starting = running(&tables, ("starting", "early"), (1_000, false));
    let mut whole = running(&tables, ("whole", "early"), (1_000, false));
    whole.booked = Booked::WholeHost;
    let mut freed = running(&tables, ("freed", "early"), (1_000, false));
    freed.step = AllocationStep::Freed;
    let live = [started.clone(), starting.clone(), whole, freed];
    let ids = |names: &[&str]| -> Vec<JobId> {
        names
            .iter()
            .map(|name| JobId::parse(name).unwrap())
            .collect()
    };
    let unread = placer.engine().sync_live_curves(&live);
    assert_eq!(unread, ids(&["started", "starting"]));
    placer.read_live_curves(&tables.jobs, &live);
    assert_eq!(placer.engine().sync_live_curves(&live), ids(&["starting"]));
    let held: Vec<(JobId, Option<u64>)> = {
        let engine = placer.engine();
        let held = engine.book.live.iter();
        held.map(|(job, held)| (job.clone(), held.started_ms))
            .collect()
    };
    let started_at = tables
        .jobs
        .get_job(&started.job)
        .unwrap()
        .unwrap()
        .started_at;
    let started_ms = started_at.map(|at| at.as_duration().as_millis());
    assert!(started_ms.is_some_and(|ms| ms >= u128::from(NOW_MS - 30_000)));
    assert_eq!(
        held,
        [
            (
                JobId::parse("started").unwrap(),
                started_ms.map(|ms| u64::try_from(ms).unwrap())
            ),
            (JobId::parse("starting").unwrap(), None)
        ]
    );

    let mut again = started;
    again.allocation_id = AllocationId::from_bits([9; 16]);
    let live = [again, starting];
    assert_eq!(
        placer.engine().sync_live_curves(&live),
        ids(&["started", "starting"])
    );
    assert_eq!(placer.engine().sync_live_curves(&[]), Vec::new());
    assert!(placer.engine().book.live.is_empty());
}

/// Asserts that nothing is read or kept of the live jobs under `off`: the pass reads no job
/// record (the store sees no read) and the engine keeps no live job. Catches a store read added
/// to the mode that learns nothing.
#[test]
fn off_reads_no_live_job() {
    let tables = Tables::new();
    let (placer, _) = placer_in(Curves::Off, CurveSettings::default());
    let row = running(&tables, ("started", "early"), (1_000, true));
    let _ = tables.store.take_reads();
    placer.read_live_curves(&tables.jobs, &[row]);
    assert_eq!(tables.store.take_reads(), Vec::new());
    assert!(placer.engine().book.live.is_empty());
    assert_eq!(placer.memory_shadow_view(), None);
}

/// Asserts what `shadow` keeps of a live job, for the memory shadow to project: its record is
/// read once, and the engine holds its flat CPU and memory, its class for its CPU (it left its
/// CPU at 0) and none for its memory (it named 64 MiB, so it is projected at that), and its
/// start. Catches a shadow that projects no live job, one that keeps a job's memory as anything
/// but what its allocation booked, and one that projects a named memory by its class.
#[test]
fn shadow_keeps_each_live_jobs_memory_class_and_start() {
    let tables = Tables::new();
    let (placer, _) = placer_in(Curves::Shadow, CurveSettings::default());
    let row = running(&tables, ("started", "early"), (1_000, true));
    let _ = tables.store.take_reads();
    placer.read_live_curves(&tables.jobs, &[row]);
    assert_eq!(tables.store.take_reads().len(), 1);
    let engine = placer.engine();
    let held = &engine.book.live[&JobId::parse("started").unwrap()];
    assert_eq!((held.flat_millis, held.memory_bytes), (1_000, 64 * MIB));
    assert!(held.memory_key.is_none() & held.key.is_some());
    assert!(held.started_ms.is_some());
    drop(engine);
    assert!(placer.memory_shadow_view().is_some());
}

/// Asserts what a job whose record cannot be read is charged: a read of the store that fails is
/// logged at warn with the job and the job is charged its flat number at every slot (the class
/// and the start stay unknown); a job the store does not hold is charged the same without a line.
/// Catches a failed read that stops the pass or leaves the job uncharged.
#[test]
fn a_live_job_that_cannot_be_read_is_charged_its_flat_number() {
    let tables = Tables::new();
    let other = Tables::new();
    let (placer, _) = placer_in(Curves::On, CurveSettings::default());
    teach(
        placer.engine().sizes(),
        &tables,
        ("early", 24),
        &burst_at(20, 352, 140),
    );
    let unread = running(&tables, ("bad", "early"), (1_000, true));
    let elsewhere = running(&other, ("elsewhere", "early"), (2_000, true));
    let (lines, _guard) = capture_targets();
    tables.store.fail_read_after(0);
    placer.read_live_curves(&tables.jobs, &[unread, elsewhere]);
    let lines = lines.taken();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(" WARN js_sched_placer::engine::curve_room: a live job was not read; it is charged its flat number job=bad"),
        "{}",
        lines[0]
    );
    let engine = placer.engine();
    let table = engine.table_of(&host("a"), at(NOW_MS));
    assert_eq!(table.charges, vec![3_000; js_sizing::CURVE_SLOTS]);
    assert!(
        engine
            .book
            .live
            .values()
            .all(|held| held.key.is_none() & held.started_ms.is_none())
    );
}
