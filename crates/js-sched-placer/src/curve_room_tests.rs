//! What the engine decides under `--placement-curves on` ([`Engine::by_curve`] and the tables
//! under it): hosts of 88 cores (96 800 m under the default slack) whose live jobs are charged
//! by the classes' curves, walked at a farm time the test sets. A class `early` bursts to 22
//! cores (a quarter of the host) from its 20th second, `late` from its 60th, and idles at a
//! quarter core otherwise; each is taught 24 runs, which is what it takes to pass the checks.

use std::collections::BTreeMap;

use js_jobs::{AllocationId, Booked, JobId, JobRecord};
use js_sched::{LoadConfig, RefusalKind};
use js_sizing::{CURVE_SLOTS, TAIL_SLOT};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Resources, Shape};

use super::{LiveJob, leaves_cpu, residual};
use crate::curves::{CurveHosts, CurveSettings};
use crate::engine::jobs::OpenHost;
use crate::engine::rig::{Tables, at, host};
use crate::engine::{Engine, Settings};
use crate::sizes::Curves;
use crate::sizes::curve_rig::{
    burst_at, busy_until, capture_targets, queued_falling_back, queued_of, run_booking, teach,
    without_series,
};

/// The farm time of the walks, in milliseconds.
const NOW_MS: u64 = 5_000_000;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The cores a host of the tests has, times the slack: 96 800 m.
const LIMIT: u64 = 96_800;

/// A host of 88 cores and 256 GiB.
fn shape() -> Shape {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    Shape::new(Resources::new(88_000, 256 * GIB, 0, None).unwrap(), caps)
}

/// An engine in mode `curves` that books by curve as `curve` says, over the hosts `names`.
fn engine_in(settings: Settings, curve: CurveSettings, names: &[&str]) -> Engine {
    let mut engine = Engine::build(settings, at(0)).with_curve_settings(curve);
    for name in names {
        engine.host_up(&host(name), &shape(), at(0));
    }
    engine
}

/// The settings of mode `curves`.
fn mode(curves: Curves) -> Settings {
    Settings {
        curves,
        ..Settings::default()
    }
}

/// Teaches the engine's history the classes `early` and `late` (24 runs each).
fn teach_bursts(engine: &mut Engine, tables: &Tables) {
    teach(
        engine.sizes(),
        tables,
        ("early", 24),
        &burst_at(20, 352, 140),
    );
    teach(
        engine.sizes(),
        tables,
        ("late", 24),
        &burst_at(60, 352, 140),
    );
}

/// The live allocation `name` of `flat` millicores on `on`, as the pass holds it: of the class of
/// `class` (`None`: its record is not read) and `age_ms` old at [`NOW_MS`] (`None`: not started).
fn live(
    engine: &mut Engine,
    on: &str,
    (name, flat): (&str, u64),
    (class, age_ms): (Option<&JobRecord>, Option<u64>),
) {
    let (job, on) = (JobId::parse(name).unwrap(), host(on));
    let booked = Booked::Shape {
        cpu_millis: flat,
        memory_bytes: 64 * MIB,
        disk_bytes: 0,
        accelerators: 0,
    };
    engine.take_job(&job, &on, booked, Vec::new());
    let mut bits = [0u8; 16];
    let bytes = name.as_bytes();
    bits[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
    let held = LiveJob {
        allocation: AllocationId::from_bits(bits),
        host: on,
        flat_millis: flat,
        key: class.map(crate::sizes::JobSizes::key_of),
        started_ms: age_ms.map(|age| NOW_MS - age),
    };
    engine.book.live.insert(job, held);
}

/// A reading numbered `seq` of a host that has used `used` millicore-windows since its agent
/// started (the difference of two readings is the cores busy over their window, in millicores).
fn reading(seq: u64, used: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: used * 5_000,
        memory_working_set_bytes: GIB,
        ..NodeLoad::default()
    }
}

/// Walks `queued` over the hosts `names`, all open and each heard from at `now_ms` first: where
/// each job was placed and the jobs that were not, with whether a host could run them.
fn walk(
    engine: &mut Engine,
    queued: &[JobRecord],
    (names, now_ms): (&[&str], u64),
) -> (BTreeMap<JobId, HostId>, Vec<(JobId, bool)>) {
    for name in names {
        engine.heard(&host(name), None, at(now_ms));
    }
    let open = names
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect();
    let (decided, waiting) = engine.walk_jobs(queued, &open, at(now_ms));
    let placed = decided.into_iter().map(|(job, host, _, _)| (job, host));
    (placed.collect(), waiting)
}

/// The refusal of `kind` the last walk booked for `on`: the job, what the host counted, what the
/// job added and the limit.
fn refusal(engine: &mut Engine, on: &str, kind: RefusalKind) -> Option<(String, u64, u64, u64)> {
    let refused = engine.take_refused();
    let (job, figures) = refused.latest(&host(on), kind)?;
    Some((job.to_string(), figures.counted, figures.new, figures.limit))
}

/// Each host of `names`, with three jobs of 20 000 m with no class (60 000 m) and a job of `early`
/// a second old; `early` and `late` taught. A job of either, flat, is 1000 m.
fn bursts(settings: Settings, curve: CurveSettings, names: &[&str], tables: &Tables) -> Engine {
    let mut engine = engine_in(settings, curve, names);
    teach_bursts(&mut engine, tables);
    let early = queued_of(tables, "model", "early");
    for name in names {
        for at in 0..3 {
            let fill = (&format!("{name}-fill-{at}")[..], 20_000);
            live(&mut engine, name, fill, (None, None));
        }
        let running = (&format!("{name}-running")[..], 1_000);
        live(&mut engine, name, running, (Some(&early), Some(1_000)));
    }
    engine
}

/// Asserts the packing rule: on a host with 60 000 m of jobs and a job of `early` a second old
/// (whose burst of 22 cores comes at 20 s), a second job of `early` is refused as `cpu-curve`
/// (its burst would meet the other's at the 10th slot: 82 000 m counted, 22 000 m new, over the
/// 96 800 m limit) and takes no host, while a job of `late` (whose burst comes at 60 s) is placed;
/// and the flat numbers would take both (the same host in shadow places the job of `early`:
/// 62 000 m of flat bookings with it). The first slot over is the one the figures name. Catches a
/// packing that sums flat numbers, a refusal at the wrong slot or with another kind, and bursts
/// of one class that are not kept apart.
#[test]
fn a_host_refuses_a_second_job_whose_burst_would_meet_the_first_and_takes_one_whose_burst_is_later()
{
    let tables = Tables::new();
    let mut engine = bursts(mode(Curves::On), CurveSettings::default(), &["a"], &tables);
    let early = queued_of(&tables, "new-early", "early");
    let (placed, waiting) = walk(&mut engine, &[early], (&["a"], NOW_MS));
    assert_eq!(placed, BTreeMap::new());
    assert_eq!(waiting, vec![(JobId::parse("new-early").unwrap(), true)]);
    let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve).unwrap();
    assert_eq!(figures, ("new-early".to_owned(), 82_000, 22_000, LIMIT));
    assert_eq!(engine.curve_view().unwrap().host_peaks[&host("a")], 82_000);

    let late = queued_of(&tables, "new-late", "late");
    let (placed, waiting) = walk(&mut engine, &[late], (&["a"], NOW_MS));
    assert_eq!(
        placed,
        BTreeMap::from([(JobId::parse("new-late").unwrap(), host("a"))])
    );
    assert_eq!(waiting, Vec::new());
    assert_eq!(
        engine
            .take_refused()
            .count(&host("a"), RefusalKind::CpuCurve),
        0
    );
    assert_eq!(engine.curve_view().unwrap().host_peaks[&host("a")], 82_250);

    for curves in [Curves::Off, Curves::Shadow] {
        let tables = Tables::new();
        let mut flat = bursts(mode(curves), CurveSettings::default(), &["a"], &tables);
        let early = queued_of(&tables, "flat-early", "early");
        let (placed, _) = walk(&mut flat, std::slice::from_ref(&early), (&["a"], NOW_MS));
        assert_eq!(
            placed,
            BTreeMap::from([(early.id, host("a"))]),
            "{curves:?}"
        );
    }
}

/// The same walk in each mode: the host named by `engine`'s `curves` places the jobs of a queue
/// over three hosts (live jobs, readings above and below their bookings, a class of 10 runs that
/// has not passed its checks, a cold class, jobs that name their CPU), and the jobs that wait.
fn farm(curves: Curves) -> (BTreeMap<JobId, HostId>, Vec<(JobId, bool)>) {
    let tables = Tables::new();
    let mut engine = engine_in(mode(curves), CurveSettings::default(), &["a", "b", "c"]);
    teach(
        engine.sizes(),
        &tables,
        ("young", 10),
        &burst_at(20, 352, 140),
    );
    for (on, name, flat) in [
        ("a", "a-0", 20_000),
        ("a", "a-1", 20_000),
        ("b", "b-0", 10_000),
        ("c", "c-0", 40_000),
        ("c", "c-1", 40_000),
    ] {
        live(&mut engine, on, (name, flat), (None, None));
    }
    for (on, seq, used) in [("a", 1, 0), ("a", 2, 50_000), ("c", 1, 0), ("c", 2, 85_000)] {
        let at_ms = NOW_MS - 5_000 * (2 - (seq - 1));
        engine.heard(&host(on), Some(&reading(seq, used)), at(at_ms));
    }
    let named = |id: &str, cpu: u64| {
        let needs = js_jobs::Needs::Shape(Resources::new(cpu, MIB, 0, None).unwrap());
        tables.submit_with(id, needs, 600)
    };
    let queued = [
        named("n0", 30_000),
        named("n1", 40_000),
        queued_of(&tables, "n2", "young"),
        queued_of(&tables, "n3", "cold"),
        named("n4", 50_000),
        named("n5", 5_000),
        named("n6", 60_000),
    ];
    walk(&mut engine, &queued, (&["a", "b", "c"], NOW_MS))
}

/// Asserts that `on` with no class past its checks places as `shadow` and `off` do, job for job:
/// seven jobs over three hosts of 88 cores (one holding 40 000 m of jobs under a reading of
/// 50 000 m, one 10 000 m with no reading, one 80 000 m under a reading of 85 000 m) go to the
/// same hosts, and the same ones wait. Catches a curve rule that moves a flat class, and a
/// residual or a pack share that differs from the scheduler's own.
#[test]
fn on_with_every_class_flat_places_as_shadow_and_off_do() {
    let off = farm(Curves::Off);
    assert_eq!(off.0.len() + off.1.len(), 7);
    assert!(
        !off.0.is_empty() & !off.1.is_empty(),
        "some placed, some wait: {off:?}"
    );
    assert_eq!(farm(Curves::Shadow), off);
    assert_eq!(farm(Curves::On), off);
}

/// Asserts the canary: with the curve hosts naming `a` alone, `a` refuses a job of `early` by its
/// curve (`cpu-curve`, 82 000 m counted) and `b`, identical to it, judges the job as today (its
/// flat 1000 m beside 63 000 m of jobs) and takes it, with no `cpu-curve` refusal of its own.
/// Catches a curve rule applied to a host the settings do not name.
#[test]
fn only_the_hosts_the_settings_name_are_judged_by_curve() {
    let tables = Tables::new();
    let curve = CurveSettings {
        hosts: CurveHosts::new([host("a")].into_iter().collect()),
        ..CurveSettings::default()
    };
    let mut engine = bursts(mode(Curves::On), curve, &["a", "b"], &tables);
    let early = queued_of(&tables, "new-early", "early");
    let (placed, waiting) = walk(
        &mut engine,
        std::slice::from_ref(&early),
        (&["a", "b"], NOW_MS),
    );
    assert_eq!(placed, BTreeMap::from([(early.id, host("b"))]));
    assert_eq!(waiting, Vec::new());
    let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve);
    assert_eq!(
        figures,
        Some(("new-early".to_owned(), 82_000, 22_000, LIMIT))
    );
    assert_eq!(
        engine
            .take_refused()
            .count(&host("b"), RefusalKind::CpuCurve),
        0
    );
    let peaks = engine.curve_view().unwrap().host_peaks;
    assert_eq!(peaks.keys().collect::<Vec<_>>(), [&host("a")]);
}

/// A class `steady` that uses 4 cores for its first 100 s and a quarter core after, over runs of
/// 140 s: 3000 m flat, and a quarter core booked once a job is 100 s old.
fn steady(engine: &mut Engine, tables: &Tables) -> JobRecord {
    teach(
        engine.sizes(),
        tables,
        ("steady", 24),
        &busy_until(100, 64, 140),
    );
    queued_of(tables, "model-steady", "steady")
}

/// Thirty-two jobs of `steady` of 3000 m each, 110 s old, on `names`.
fn old_steady(engine: &mut Engine, names: &[&str], model: &JobRecord) {
    for name in names {
        for at in 0..32 {
            let job = (&format!("{name}-old-{at:02}")[..], 3_000);
            live(engine, name, job, (Some(model), Some(110_000)));
        }
    }
}

/// Asserts that the curve uses what a past burst frees: a host holding 32 jobs of `steady` of 3000
/// m (96 000 m flat) that are past their first 100 s (a quarter core each now and after) takes
/// another of 3000 m, whose curve (4000 m for 100 s) fits every slot beside 8000 m, although the
/// flat numbers with it are 99 000 m, over the 96 800 m limit; in shadow the same host refuses it
/// by the scheduler's own `cpu` test, and a host the curve hosts do not name refuses it the same
/// way though the curve would fit. Catches a rule that keeps the flat sum beside the curve, and
/// one that judges by curve a host the settings leave out.
#[test]
fn a_job_whose_curve_fits_every_slot_is_placed_past_the_flat_sum_on_a_curve_host() {
    let tables = Tables::new();
    let mut engine = engine_in(mode(Curves::On), CurveSettings::default(), &["a"]);
    let model = steady(&mut engine, &tables);
    old_steady(&mut engine, &["a"], &model);
    let next = queued_of(&tables, "next", "steady");
    let (placed, _) = walk(&mut engine, std::slice::from_ref(&next), (&["a"], NOW_MS));
    assert_eq!(placed, BTreeMap::from([(next.id.clone(), host("a"))]));
    assert_eq!(
        engine.curve_view().unwrap().host_peaks[&host("a")],
        8_000 + 4_000
    );

    let outside = CurveSettings {
        hosts: CurveHosts::new([host("z")].into_iter().collect()),
        ..CurveSettings::default()
    };
    for (settings, curve) in [
        (mode(Curves::Shadow), CurveSettings::default()),
        (mode(Curves::On), outside),
    ] {
        let tables = Tables::new();
        let mut engine = engine_in(settings, curve, &["a"]);
        let model = steady(&mut engine, &tables);
        old_steady(&mut engine, &["a"], &model);
        let next = queued_of(&tables, "next", "steady");
        let (placed, waiting) = walk(&mut engine, std::slice::from_ref(&next), (&["a"], NOW_MS));
        assert_eq!(placed, BTreeMap::new());
        assert_eq!(waiting, vec![(next.id, true)]);
        let refused = engine.take_refused();
        assert_eq!(refused.count(&host("a"), RefusalKind::Cpu), 1);
        assert_eq!(refused.count(&host("a"), RefusalKind::CpuCurve), 0);
    }
}

/// Asserts the table of a host job by job: a running job of a class whose runs all end by 40 s
/// (8 cores for their first 20 s, then a quarter core) that is 10 s old is charged 8000 m for the
/// 5 slots of the burst left, 250 m for the 10 slots after it that its class's runs reached, and
/// 0 from the 16th slot on and in the slot from 128 s on, which none reached; a job
/// allocated and not yet started is charged as one at age 0 (its burst at the 10th slot); one 30
/// s old, past its class's burst, a quarter core at every slot; a job whose record is not read
/// (no class) and one whose class fails a check (11 usable series of 16) its flat number at
/// every slot. Catches the survival weight lost (a job charged past the end of its runs), an
/// unstarted job aged, and a flat class charged a curve.
#[test]
fn a_running_job_is_charged_at_its_age_by_how_many_of_its_class_ran_on() {
    let tables = Tables::new();
    let mut engine = engine_in(
        mode(Curves::On),
        CurveSettings::default(),
        &["a", "b", "c", "d"],
    );
    teach_bursts(&mut engine, &tables);
    teach(
        engine.sizes(),
        &tables,
        ("short", 24),
        &busy_until(20, 128, 40),
    );
    teach(
        engine.sizes(),
        &tables,
        ("weak", 24),
        &burst_at(20, 352, 140),
    );
    for at in 0..5 {
        let run = without_series(&tables, &format!("weak-none-{at}"), "weak");
        assert!(engine.sizes().observe(&run, run_booking()));
    }
    let (short, early, weak) = (
        queued_of(&tables, "m-short", "short"),
        queued_of(&tables, "m-early", "early"),
        queued_of(&tables, "m-weak", "weak"),
    );
    live(
        &mut engine,
        "a",
        ("short", 8_000),
        (Some(&short), Some(10_000)),
    );
    live(&mut engine, "b", ("unstarted", 1_000), (Some(&early), None));
    live(
        &mut engine,
        "b",
        ("past", 1_000),
        (Some(&early), Some(30_000)),
    );
    live(&mut engine, "c", ("unread", 1_000), (None, None));
    live(&mut engine, "d", ("weak", 1_000), (Some(&weak), Some(0)));
    let bounds = crate::sizes::bounds_of(engine.capacities());
    engine.sizes().set_bounds(bounds);
    let charges = |engine: &Engine, on: &str| engine.table_of(&host(on), at(NOW_MS)).charges;

    let short = charges(&engine, "a");
    assert_eq!(short.len(), CURVE_SLOTS);
    let ahead = |from: usize, to: usize| short[from..to].to_vec();
    assert_eq!(ahead(0, 5), vec![8_000; 5], "the 5 slots of its burst left");
    assert_eq!(
        ahead(5, 15),
        vec![250; 10],
        "the 10 after it, to its class's end"
    );
    assert_eq!(
        ahead(15, CURVE_SLOTS),
        vec![0; CURVE_SLOTS - 15],
        "no run went on"
    );
    let both = charges(&engine, "b");
    assert_eq!(
        both[0],
        250 + 250,
        "the unstarted at age 0, the old one past its burst"
    );
    assert_eq!(both[10], 22_000 + 250);
    assert_eq!(both[11], 22_000 + 250);
    assert_eq!(both[12], 250 + 250);
    assert_eq!(both[TAIL_SLOT], 250 + 250);
    assert_eq!(charges(&engine, "c"), vec![1_000; CURVE_SLOTS]);
    assert_eq!(charges(&engine, "d"), vec![1_000; CURVE_SLOTS]);
}

/// Asserts the checks in the placer on their boundary: a class whose last 16 runs held 12 usable
/// series (the burst class after 4 runs with none) is booked by its curve (a second job of it is
/// refused beside the running one: the bursts meet) and the same class at 11 (a fifth run with
/// none) is booked flat (the same job is placed: 62 000 m of flat numbers). Catches a check 4
/// the placer does not read.
#[test]
fn a_class_with_11_usable_series_books_flat_and_with_12_its_curve() {
    for (runs_without, placed) in [(4, false), (5, true)] {
        let tables = Tables::new();
        let mut engine = bursts(mode(Curves::On), CurveSettings::default(), &["a"], &tables);
        for at in 0..runs_without {
            let run = without_series(&tables, &format!("early-none-{at}"), "early");
            assert!(engine.sizes().observe(&run, run_booking()));
        }
        let early = queued_of(&tables, "new-early", "early");
        let (done, _) = walk(&mut engine, std::slice::from_ref(&early), (&["a"], NOW_MS));
        assert_eq!(
            done.contains_key(&early.id),
            placed,
            "{runs_without} runs with no series"
        );
    }
}

/// Asserts the pure parts of the residual, each term on its own: the scheduler's slot-0 charge
/// beyond the jobs' flat numbers, the reading and the VMs beyond the jobs' charges, and the
/// larger of the two, with no term below 0. Catches a term dropped, two summed, and a
/// subtraction that wraps.
#[test]
fn the_residual_is_the_larger_of_what_the_scheduler_and_the_reading_show_beyond_the_jobs() {
    assert_eq!(residual(100, 40, (30, 5), 20), 60, "the scheduler's charge");
    assert_eq!(
        residual(100, 90, (80, 10), 20),
        70,
        "the reading and the VMs"
    );
    assert_eq!(residual(45, 40, (30, 15), 40), 5, "both 5");
    assert_eq!(residual(10, 40, (3, 3), 20), 0, "both below");
    assert_eq!(
        residual(0, 0, (u64::MAX, u64::MAX), 0),
        u64::MAX,
        "saturating"
    );
}

/// A host `a` holding 1000 m of one job, a reading of `used` busy cores (millicores) over its
/// last window and one before it that saw none, and the class `early` taught.
fn read_at(used: u64, tables: &Tables) -> Engine {
    let mut engine = engine_in(mode(Curves::On), CurveSettings::default(), &["a"]);
    teach_bursts(&mut engine, tables);
    live(&mut engine, "a", ("held", 1_000), (None, None));
    engine.heard(&host("a"), Some(&reading(1, 0)), at(NOW_MS - 5_000));
    engine.heard(&host("a"), Some(&reading(2, used)), at(NOW_MS));
    engine
}

/// Asserts that a reading above the jobs' charges is carried at every slot: a host holding a
/// job of 1000 m under a reading of 80 000 m has a residual of 79 000 m, so a job of `early`,
/// whose curve fits the 1000 m (a burst of 22 000 m) with 80 000 m over every slot, is refused
/// as `cpu-curve` at the 10th slot with 80 000 m counted; the same host with a reading of
/// 60 000 m places it (82 000 m at the burst). Catches a residual that fades, one dropped, and
/// one added to the jobs' charges twice.
#[test]
fn what_a_reading_shows_beyond_the_jobs_is_carried_at_every_slot() {
    let tables = Tables::new();
    let mut engine = read_at(80_000, &tables);
    let table = engine.table_of(&host("a"), at(NOW_MS));
    assert_eq!(
        (table.residual, table.charges[0], table.limit),
        (79_000, 1_000, LIMIT)
    );
    let early = queued_of(&tables, "new-early", "early");
    let (placed, _) = walk(&mut engine, &[early], (&["a"], NOW_MS));
    assert_eq!(placed, BTreeMap::new());
    let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve);
    assert_eq!(
        figures,
        Some(("new-early".to_owned(), 80_000, 22_000, LIMIT))
    );

    let tables = Tables::new();
    let mut engine = read_at(60_000, &tables);
    let early = queued_of(&tables, "new-early", "early");
    let (placed, _) = walk(&mut engine, std::slice::from_ref(&early), (&["a"], NOW_MS));
    assert_eq!(placed, BTreeMap::from([(early.id, host("a"))]));
}

/// Asserts that a reading below the jobs' charges leaves no residual: a host holding 60 000 m of
/// jobs and a job of `early` a second old (82 000 m at the burst), under a reading of 2000 m, is
/// charged only its jobs: the refusal of a second job of `early` counts 82 000 m, as it does with
/// no reading. Catches a residual that goes negative or that adds the reading to the charges.
#[test]
fn a_reading_below_the_charges_adds_nothing() {
    let tables = Tables::new();
    let mut engine = bursts(mode(Curves::On), CurveSettings::default(), &["a"], &tables);
    engine.heard(&host("a"), Some(&reading(1, 0)), at(NOW_MS - 5_000));
    engine.heard(&host("a"), Some(&reading(2, 2_000)), at(NOW_MS));
    assert_eq!(engine.table_of(&host("a"), at(NOW_MS)).residual, 0);
    let early = queued_of(&tables, "new-early", "early");
    walk(&mut engine, &[early], (&["a"], NOW_MS));
    let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve);
    assert_eq!(
        figures,
        Some(("new-early".to_owned(), 82_000, 22_000, LIMIT))
    );
}

/// Asserts the limit of a table: the host's cores times the CPU slack, at the default 1.1 and at
/// 1.0. Catches a limit that ignores the slack setting.
#[test]
fn the_limit_of_a_host_is_its_cores_times_the_cpu_slack() {
    for (permille, limit) in [(100, 96_800), (0, 88_000)] {
        let load = LoadConfig {
            s_cpu_permille: permille,
            ..LoadConfig::default()
        };
        let settings = Settings {
            load,
            ..mode(Curves::On)
        };
        let engine = engine_in(settings, CurveSettings::default(), &["a"]);
        assert_eq!(engine.table_of(&host("a"), at(NOW_MS)).limit, limit);
    }
}

/// A host of `names` with 90 000 m of jobs and `early` taught: a job of `early` (1000 m flat)
/// fits flat (91 000 m of 96 800) and not by curve (112 000 m at its burst).
fn packed(max_wait_ms: u64, tables: &Tables) -> Engine {
    let curve = CurveSettings {
        max_wait: std::time::Duration::from_millis(max_wait_ms),
        ..CurveSettings::default()
    };
    let mut engine = engine_in(mode(Curves::On), curve, &["a"]);
    teach_bursts(&mut engine, tables);
    for at in 0..3 {
        live(
            &mut engine,
            "a",
            (&format!("fill-{at}"), 30_000),
            (None, None),
        );
    }
    engine
}

/// Asserts the wait bound to the millisecond on the farm clock: a job of `early` that no host
/// fits by its curve waits at the walk that finds it (the wait starts there) and at a walk 29
/// 999 ms later, and is placed, tested with its flat number, at 30 000 ms; it is counted once,
/// logged once under the target of the profile lines with its job, class and the wait, and its
/// wait is forgotten once placed. Catches a bound off by one millisecond, a wait that starts
/// again at each walk, and an expiry not counted or logged exactly once.
#[test]
fn a_job_is_tested_with_its_flat_number_after_exactly_the_wait_bound() {
    let tables = Tables::new();
    let mut engine = packed(30_000, &tables);
    let (lines, _guard) = capture_targets();
    let early = queued_of(&tables, "new-early", "early");
    for (waited, placed) in [(0, false), (29_999, false), (30_000, true)] {
        let (done, waiting) = walk(
            &mut engine,
            std::slice::from_ref(&early),
            (&["a"], NOW_MS + waited),
        );
        assert_eq!(done.contains_key(&early.id), placed, "after {waited} ms");
        assert_eq!(waiting.len(), usize::from(!placed), "after {waited} ms");
        let expired = engine.take_curve_expiries();
        assert_eq!(expired, u64::from(placed), "after {waited} ms");
        let held = engine.book.waits.get(&early.id).copied();
        assert_eq!(held, (!placed).then(|| at(NOW_MS)), "after {waited} ms");
    }
    assert_eq!(
        lines.taken(),
        [
            " INFO js_sched_placer::sizes::profile: a job's curve found no host within the wait bound; it is tested with its flat number job=new-early class=early waited_ms=30000"
        ]
    );
    assert!(engine.book.expired.is_empty());
}

/// Asserts that the wait bound counts the time no host fitted the curve, not the time since the
/// first such walk: a job of `early` whose curve fits no host at the first walk starts its wait;
/// at a walk 10 s later its curve fits the host (the jobs that filled it ended) but the host's
/// memory refuses it (a job holding all of it), and the wait ends there; at a walk 31 s after the
/// first the host is full by CPU again, and the job waits from then, not expired. Catches a wait
/// kept across a walk in which the curve fitted, which would test a job flat after 11 s with no
/// fit.
#[test]
fn a_walk_in_which_the_curve_fits_ends_the_wait() {
    let tables = Tables::new();
    let mut engine = packed(30_000, &tables);
    let early = queued_of(&tables, "new-early", "early");
    let queued = std::slice::from_ref(&early);
    let (done, _) = walk(&mut engine, queued, (&["a"], NOW_MS));
    assert!(done.is_empty());
    assert_eq!(engine.book.waits.get(&early.id), Some(&at(NOW_MS)));
    let _ = engine.take_refused();

    for at in 0..3 {
        let fill = JobId::parse(&format!("fill-{at}")).unwrap();
        engine.job_failed(&fill);
        engine.book.live.remove(&fill);
    }
    let hog = Booked::Shape {
        cpu_millis: 250,
        memory_bytes: 256 * GIB,
        disk_bytes: 0,
        accelerators: 0,
    };
    let hog_id = JobId::parse("hog").unwrap();
    engine.take_job(&hog_id, &host("a"), hog, Vec::new());
    let (done, _) = walk(&mut engine, queued, (&["a"], NOW_MS + 10_000));
    assert!(done.is_empty(), "refused by memory");
    assert!(
        refusal(&mut engine, "a", RefusalKind::CpuCurve).is_none(),
        "the curve fitted"
    );
    assert_eq!(engine.book.waits.get(&early.id), None, "the wait ended");

    engine.job_failed(&hog_id);
    for at in 0..3 {
        live(
            &mut engine,
            "a",
            (&format!("fill-{at}"), 30_000),
            (None, None),
        );
    }
    let (done, _) = walk(&mut engine, queued, (&["a"], NOW_MS + 31_000));
    assert!(done.is_empty());
    assert_eq!(engine.take_curve_expiries(), 0, "not expired");
    assert_eq!(engine.book.waits.get(&early.id), Some(&at(NOW_MS + 31_000)));
}

/// Asserts that a wait bound of 0 never delays a job: a job whose curve fits no host is tested
/// with its flat number in the same walk and placed, counted once. Catches a bound of 0 that
/// still waits for a later walk.
#[test]
fn a_wait_bound_of_0_places_a_job_the_curve_fits_nowhere_in_the_same_walk() {
    let tables = Tables::new();
    let mut engine = packed(0, &tables);
    let early = queued_of(&tables, "new-early", "early");
    let (done, _) = walk(&mut engine, std::slice::from_ref(&early), (&["a"], NOW_MS));
    assert_eq!(done, BTreeMap::from([(early.id, host("a"))]));
    assert_eq!(engine.take_curve_expiries(), 1);
    assert!(engine.book.waits.is_empty());
}

/// Asserts what stays of an expired wait: a job whose flat number fits no host either (96 500 m of
/// jobs) is counted once at the walk its bound runs out and not again, and from then on it is
/// refused as a flat job (its refusal's new figure is its 1000 m at the first slot, not its
/// burst); and a job the queue no longer holds loses its wait, so one that returns waits again
/// from its return. Catches an expiry counted at every walk, a curve tested again after the bound,
/// and a wait kept for a job that left the queue.
#[test]
fn an_expired_job_stays_flat_and_a_job_that_left_the_queue_waits_again() {
    let tables = Tables::new();
    let mut engine = packed(30_000, &tables);
    live(&mut engine, "a", ("more", 6_500), (None, None));
    let early = queued_of(&tables, "new-early", "early");
    let walks = [(0, 0), (30_000, 1), (31_000, 0)];
    for (waited, expired) in walks {
        let (done, _) = walk(
            &mut engine,
            std::slice::from_ref(&early),
            (&["a"], NOW_MS + waited),
        );
        assert_eq!(done, BTreeMap::new());
        assert_eq!(engine.take_curve_expiries(), expired, "after {waited} ms");
        let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve);
        let new = if waited == 0 { 22_000 } else { 1_000 };
        assert_eq!(figures.map(|f| f.2), Some(new), "after {waited} ms");
    }
    walk(&mut engine, &[], (&["a"], NOW_MS + 40_000));
    assert!(engine.book.waits.is_empty() & engine.book.expired.is_empty());
    walk(
        &mut engine,
        std::slice::from_ref(&early),
        (&["a"], NOW_MS + 50_000),
    );
    assert_eq!(engine.take_curve_expiries(), 0);
    assert_eq!(engine.book.waits[&early.id], at(NOW_MS + 50_000));
    let figures = refusal(&mut engine, "a", RefusalKind::CpuCurve);
    assert_eq!(figures.map(|f| f.2), Some(22_000), "a curve again");
}

/// Asserts that a job that names its CPU books it flat (no curve, even of a class that passes its
/// checks), and that a job whose own class is cold books flat though the class it falls back to
/// is shaped. Catches a curve applied to a named CPU and a curve read from a fallback class.
#[test]
fn only_a_job_that_leaves_its_cpu_at_0_books_the_curve_of_its_own_class() {
    let tables = Tables::new();
    let mut engine = bursts(mode(Curves::On), CurveSettings::default(), &["a"], &tables);
    let named = js_jobs::Needs::Shape(Resources::new(1_000, MIB, 0, None).unwrap());
    let named = tables.submit_with("named", named, 600);
    let (placed, _) = walk(&mut engine, std::slice::from_ref(&named), (&["a"], NOW_MS));
    assert_eq!(
        placed,
        BTreeMap::from([(named.id, host("a"))]),
        "a named CPU books flat"
    );

    let falling = queued_falling_back(&tables, "falls-back", ("cold", "early"));
    let (placed, _) = walk(
        &mut engine,
        std::slice::from_ref(&falling),
        (&["a"], NOW_MS),
    );
    assert_eq!(
        placed.get(&falling.id),
        Some(&host("a")),
        "its own class is cold: flat"
    );
}

/// Asserts which jobs leave their CPU at 0: one that names no CPU (sized from its class), also
/// when it names its memory; not one that names its CPU, nor one that books the whole host.
/// Catches a named CPU read as a request to be sized.
#[test]
fn a_job_leaves_its_cpu_at_0_unless_it_names_it_or_books_the_whole_host() {
    let tables = Tables::new();
    let shape = |cpu: u64, memory: u64| {
        let needs = js_jobs::Needs::Shape(Resources::new(cpu, memory, 0, None).unwrap());
        tables.submit_with(&format!("job-{cpu}-{memory}"), needs, 600)
    };
    assert!(leaves_cpu(&shape(0, 0)));
    assert!(leaves_cpu(&shape(0, MIB)));
    assert!(!leaves_cpu(&shape(1, 0)));
    assert!(!leaves_cpu(&tables.whole("whole")));
}

/// Asserts the view: the classes that pass their four checks and those that book flat (`early`,
/// `late` and a class of 3 runs), and the fullest booked slot of each curve host as the last walk
/// ended; and that it is `None` under `off` and `shadow`. Catches a count that reads the wrong
/// side of the checks and a peak that includes the residual.
#[test]
fn the_view_counts_the_classes_and_names_the_peak_of_each_curve_host() {
    let tables = Tables::new();
    let mut engine = bursts(
        mode(Curves::On),
        CurveSettings::default(),
        &["a", "b"],
        &tables,
    );
    teach(
        engine.sizes(),
        &tables,
        ("young", 3),
        &burst_at(20, 352, 140),
    );
    engine.heard(&host("a"), Some(&reading(1, 0)), at(NOW_MS - 5_000));
    engine.heard(&host("a"), Some(&reading(2, 70_000)), at(NOW_MS));
    walk(&mut engine, &[], (&["a", "b"], NOW_MS));
    let view = engine.curve_view().unwrap();
    assert_eq!((view.curve_classes, view.flat_classes), (2, 1));
    assert_eq!(
        view.host_peaks.values().copied().collect::<Vec<_>>(),
        [82_000, 82_000]
    );
    for curves in [Curves::Off, Curves::Shadow] {
        let engine = engine_in(mode(curves), CurveSettings::default(), &["a"]);
        assert_eq!(engine.curve_view(), None);
    }
}

/// Asserts that the pack limit reads the fullest booked slot of a curve host: with the limit at
/// 44 000 m (half of 88 cores), a host holding 21 000 m and a job of `early` (a burst of 22 000 m
/// at its 10th slot) books 44 000 m at its fullest with a new job of 1000 m, within the limit, and
/// the job goes to it, the fuller host by its flat numbers (23 000 m against 11 000 m); with a
/// new job of 1001 m the host's fullest slot is 44 001 m, past the limit, and the job goes to the
/// other host. The same hosts in shadow place both jobs on the fuller one. Catches a pack limit
/// that reads the sum of the flat numbers, and one off by a millicore.
#[test]
fn the_pack_limit_reads_the_fullest_slot_of_a_curve_host() {
    for (full, light) in [("a", "b"), ("b", "a")] {
        for (cpu, goes_to_full) in [(1_000, true), (1_001, false)] {
            let tables = Tables::new();
            let mut engine = engine_in(mode(Curves::On), CurveSettings::default(), &["a", "b"]);
            teach_bursts(&mut engine, &tables);
            let early = queued_of(&tables, "model", "early");
            live(&mut engine, full, ("full-fill", 21_000), (None, None));
            live(
                &mut engine,
                full,
                ("full-run", 1_000),
                (Some(&early), Some(1_000)),
            );
            live(&mut engine, light, ("light-fill", 10_000), (None, None));
            let needs = js_jobs::Needs::Shape(Resources::new(cpu, MIB, 0, None).unwrap());
            let job = tables.submit_with("new", needs, 600);
            let (placed, _) = walk(
                &mut engine,
                std::slice::from_ref(&job),
                (&["a", "b"], NOW_MS),
            );
            let chosen = if goes_to_full { full } else { light };
            assert_eq!(
                placed,
                BTreeMap::from([(job.id.clone(), host(chosen))]),
                "{full} full, {cpu} m"
            );

            let mut shadow = engine_in(mode(Curves::Shadow), CurveSettings::default(), &["a", "b"]);
            live(&mut shadow, full, ("full-fill", 21_000), (None, None));
            live(&mut shadow, full, ("full-run", 1_000), (None, None));
            live(&mut shadow, light, ("light-fill", 10_000), (None, None));
            let (placed, _) = walk(
                &mut shadow,
                std::slice::from_ref(&job),
                (&["a", "b"], NOW_MS),
            );
            assert_eq!(
                placed,
                BTreeMap::from([(job.id, host(full))]),
                "shadow, {cpu} m"
            );
        }
    }
}

/// Asserts what a curve host keeps of the store's capacity check: its CPU is no longer held to
/// the store's figure (a host offering 1000 m of CPU takes a job of 2000 m that the curve fits)
/// while its memory still is (a job of 8 GiB is refused as `booked-memory` by a host offering 4
/// GiB, as on any host), and in shadow the same host refuses the job of 2000 m by CPU (no
/// `cpu-curve` and no placement). Catches a curve host that skips the whole ledger, and one that
/// still holds its CPU to the store's.
#[test]
fn a_curve_host_is_held_to_the_stores_capacity_but_for_its_cpu() {
    let capacity = js_jobs::Capacity {
        cpu_millis: 1_000,
        memory_bytes: 4 * GIB,
        disk_bytes: 0,
        accelerators: 0,
    };
    let offer = OpenHost {
        capacity: Some(capacity),
        ..OpenHost::default()
    };
    let tables = Tables::new();
    let job = |id: &str, cpu: u64, memory: u64| {
        let needs = js_jobs::Needs::Shape(Resources::new(cpu, memory, 0, None).unwrap());
        tables.submit_with(id, needs, 600)
    };
    let (small, large) = (job("small", 2_000, GIB), job("large", 500, 8 * GIB));
    let open = BTreeMap::from([(host("a"), offer)]);
    for (curves, small_placed) in [(Curves::On, true), (Curves::Shadow, false)] {
        for (queued, placed) in [(&small, small_placed), (&large, false)] {
            let mut engine = engine_in(mode(curves), CurveSettings::default(), &["a"]);
            engine.set_room(crate::room::Room::of(&open, &[]));
            engine.heard(&host("a"), None, at(NOW_MS));
            let (decided, _) = engine.walk_jobs(std::slice::from_ref(queued), &open, at(NOW_MS));
            assert_eq!(
                decided.len(),
                usize::from(placed),
                "{curves:?} {}",
                queued.id
            );
            let refused = engine.take_refused();
            let by_memory = refused.count(&host("a"), RefusalKind::BookedMemory);
            assert_eq!(
                by_memory,
                u64::from(queued.id.as_str() == "large"),
                "{curves:?}"
            );
        }
    }
}
