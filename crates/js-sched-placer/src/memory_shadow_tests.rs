//! What the memory shadow records under `--placement-curves shadow` (and nothing under `off`),
//! walked at farm times the tests set over a host whose room holds it to 20 GiB of memory. A
//! class `short` holds 2 GiB for 20 s; a class `link` holds 1 GiB for 40 s and then 10 GiB until
//! its 60th second, as a compile that links at its end does; each is taught 24 runs (16 scored),
//! which is what a class needs to have a memory curve, or 8 where a test says too little history.

use std::collections::BTreeMap;

use js_jobs::{AllocationId, Booked, Capacity, JobId, JobRecord};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Resources, Shape};

use super::super::curve_room::{LiveJob, leaves_memory};
use crate::curves::{CurveSettings, HostMemoryShadow};
use crate::engine::jobs::OpenHost;
use crate::engine::rig::{Tables, at, host};
use crate::engine::{Engine, Settings};
use crate::room::Room;
use crate::sizes::curve_rig::{capture_targets, queued_of, teach_memory};
use crate::sizes::{ColdSize, Curves, JobSizes};

/// The farm time of the first walk, in milliseconds.
const NOW_MS: u64 = 5_000_000;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The memory the room holds host `a` to.
const ROOM: u64 = 20 * GIB;

/// What a job of `short` running on `a` books: 12 GiB, its flat memory booking.
const SHORT_FLAT: u64 = 12 * GIB;

/// `mib` with the margin of the flat memory booking, in bytes: `mib + ceil(mib / 5) + 128 MiB`.
fn margined(mib: u64) -> u64 {
    let bytes = mib * MIB;
    bytes + bytes.div_ceil(5) + 128 * MIB
}

/// A host of 88 cores and 256 GiB: the scheduler never refuses a job of these tests.
fn shape() -> Shape {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    Shape::new(Resources::new(88_000, 256 * GIB, 0, None).unwrap(), caps)
}

/// An engine in mode `curves` over host `a`.
fn engine_in(curves: Curves) -> Engine {
    let settings = Settings {
        curves,
        ..Settings::default()
    };
    let mut engine = Engine::build(settings, at(0)).with_curve_settings(CurveSettings::default());
    engine.host_up(&host("a"), &shape(), at(0));
    engine
}

/// The classes `short` and `link`, each taught `runs` runs.
fn taught(engine: &mut Engine, tables: &Tables, runs: usize) {
    teach_memory(engine.sizes(), tables, ("short", runs), &[2048; 20]);
    let mut link = vec![1024; 60];
    link[40..].fill(10_240);
    teach_memory(engine.sizes(), tables, ("link", runs), &link);
}

/// Host `a` as the walks offer it: held to [`ROOM`] of memory when `held` says so, else to no
/// capacity at all.
fn open(held: bool) -> BTreeMap<HostId, OpenHost> {
    let capacity = held.then_some(Capacity {
        cpu_millis: 88_000,
        memory_bytes: ROOM,
        disk_bytes: 0,
        accelerators: 0,
    });
    let offer = OpenHost {
        capacity,
        ..OpenHost::default()
    };
    BTreeMap::from([(host("a"), offer)])
}

/// The job `name` of the class of `class`, booked 1000 m and [`SHORT_FLAT`], live on `a` and
/// `age_ms` old at [`NOW_MS`], as the pass holds it: what it books.
fn running(engine: &mut Engine, name: &str, class: &JobRecord, age_ms: u64) -> Booked {
    let (job, on) = (JobId::parse(name).unwrap(), host("a"));
    let booked = Booked::Shape {
        cpu_millis: 1_000,
        memory_bytes: SHORT_FLAT,
        disk_bytes: 0,
        accelerators: 0,
    };
    engine.take_job(&job, &on, booked, Vec::new());
    let held = LiveJob {
        allocation: AllocationId::from_bits([7; 16]),
        host: on,
        flat_millis: 1_000,
        memory_bytes: SHORT_FLAT,
        key: None,
        memory_key: Some(JobSizes::key_of(class)),
        started_ms: Some(NOW_MS - age_ms),
    };
    engine.book.live.insert(job, held);
    booked
}

/// Walks `queued` over `a` at `now_ms`, its room holding `booked` (when `held`): where each job
/// was placed.
fn walk(
    engine: &mut Engine,
    queued: &[JobRecord],
    (held, booked): (bool, &[Booked]),
    now_ms: u64,
) -> BTreeMap<JobId, HostId> {
    let open = open(held);
    let mut room = Room::of(&open, &[]);
    booked
        .iter()
        .for_each(|booked| room.add(&host("a"), *booked));
    engine.set_room(room);
    engine.heard(&host("a"), None, at(now_ms));
    let (decided, _) = engine.walk_jobs(queued, &open, at(now_ms));
    decided
        .into_iter()
        .map(|(job, on, _, _)| (job, on))
        .collect()
}

/// A reading of `a` numbered `seq` that shows `used` bytes of memory in use.
fn reading(seq: u64, used: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        memory_working_set_bytes: used,
        ..NodeLoad::default()
    }
}

/// The lines of the memory shadow among `lines`.
fn shadow_lines(lines: Vec<String>) -> Vec<String> {
    let ours = |line: &String| line.contains("memory curve");
    lines.into_iter().filter(ours).collect()
}

/// A job of `link` queued while host `a` runs a job of `short` 10 s old: walked at [`NOW_MS`],
/// and again 10 s later, once the job of `short` has ended and left the room. Where each walk
/// placed it.
fn late_rise(
    engine: &mut Engine,
    tables: &Tables,
) -> (BTreeMap<JobId, HostId>, BTreeMap<JobId, HostId>) {
    let short = queued_of(tables, "short-model", "short");
    let link = queued_of(tables, "link-new", "link");
    let booked = running(engine, "short-running", &short, 10_000);
    let first = walk(
        engine,
        std::slice::from_ref(&link),
        (true, &[booked]),
        NOW_MS,
    );
    engine.book.live.clear();
    let second = walk(engine, &[link], (true, &[]), NOW_MS + 10_000);
    (first, second)
}

/// Asserts the late rise: a job of `link` whose flat booking (about 12 GiB) does not fit beside
/// the 12 GiB a running job of `short` books on a host of 20 GiB waits under the flat booking,
/// while by the curves it fits at once (the job of `short` holds about 2.5 GiB until it ends,
/// 10 s on, and the job of `link` holds about 1.3 GiB until its 40th second); placed 10 s later,
/// it is counted and logged as placed 10 000 ms sooner, with the job, its class and its host.
/// Catches a shadow that holds a running job at its flat booking to its end, one that books the
/// new job's peak from its start, and a placement that is not counted.
#[test]
fn a_job_whose_memory_rises_late_waits_by_the_flat_booking_and_would_be_placed_sooner_by_its_curve()
{
    let tables = Tables::new();
    let mut engine = engine_in(Curves::Shadow);
    taught(&mut engine, &tables, 24);
    let (lines, _guard) = capture_targets();
    let (first, second) = late_rise(&mut engine, &tables);
    assert_eq!(first, BTreeMap::new());
    let link = JobId::parse("link-new").unwrap();
    assert_eq!(second, BTreeMap::from([(link, host("a"))]));
    let view = engine.memory_shadow_view().unwrap();
    let counts = (view.same, view.sooner, view.later, view.sooner_ms);
    assert_eq!(counts, (0, 1, 0, 10_000));
    assert_eq!(
        shadow_lines(lines.taken()),
        [
            " INFO js_sched_placer::sizes::profile: placement curves: a job's memory curve would have placed it sooner job=link-new class=link host=a sooner_ms=10000"
        ]
    );
}

/// Asserts that the shadow places nothing: the late rise places the job of `link` at the same
/// walk under `off` as under `shadow` (the second), and `off` keeps no view. Catches a shadow
/// whose judgement reaches the placement.
#[test]
fn the_late_rise_is_placed_alike_under_off_and_shadow() {
    let mut placed = Vec::new();
    for curves in [Curves::Off, Curves::Shadow] {
        let tables = Tables::new();
        let mut engine = engine_in(curves);
        taught(&mut engine, &tables, 24);
        placed.push(late_rise(&mut engine, &tables));
        assert_eq!(
            engine.memory_shadow_view().is_some(),
            curves == Curves::Shadow
        );
    }
    assert_eq!(placed[0], placed[1]);
}

/// Asserts the fallback of a class with too little history: with 8 runs of each class (none
/// scored 16), the running job of `short` is projected at its flat 12 GiB and the job of `link`
/// books its flat booking at every slot, so the curve fits no sooner than the flat booking and
/// the placement counts as the same, with no line. Catches a curve read from too little history.
#[test]
fn a_class_with_too_little_history_is_projected_and_booked_flat() {
    let tables = Tables::new();
    let mut engine = engine_in(Curves::Shadow);
    taught(&mut engine, &tables, 8);
    let (lines, _guard) = capture_targets();
    let short = queued_of(&tables, "short-model", "short");
    let link = queued_of(&tables, "link-new", "link");
    let booked = running(&mut engine, "short-running", &short, 10_000);
    let first = walk(
        &mut engine,
        std::slice::from_ref(&link),
        (true, &[booked]),
        NOW_MS,
    );
    assert_eq!(first, BTreeMap::new());
    let view = engine.memory_shadow_view().unwrap();
    assert_eq!(view.hosts[&host("a")].projected_bytes, SHORT_FLAT);
    engine.book.live.clear();
    let second = walk(&mut engine, &[link], (true, &[]), NOW_MS + 10_000);
    assert_eq!(second.len(), 1);
    let view = engine.memory_shadow_view().unwrap();
    assert_eq!((view.same, view.sooner, view.later), (1, 0, 0));
    assert_eq!(shadow_lines(lines.taken()), Vec::<String>::new());
}

/// Asserts the record of a job that exceeds its profile: a running job of `short` 10 s old is
/// projected to hold 2585.6 MiB now and 2 s on; a reading 2 s after the walk that shows 6 GiB in
/// use is counted too low by what it shows beyond (and logged with both figures and how far ahead
/// the projection was), one 4 s after that shows 2 GiB is within it, and the next walk carries
/// what the job holds beyond its curve to every slot as the residual. Catches a reading compared
/// with the wrong slot, a projection that is not checked, and an excess the next walk forgets.
#[test]
fn a_job_that_exceeds_its_profile_is_recorded_as_a_projection_too_low_and_carried() {
    let tables = Tables::new();
    let mut engine = engine_in(Curves::Shadow);
    taught(&mut engine, &tables, 24);
    let short = queued_of(&tables, "short-model", "short");
    let booked = running(&mut engine, "short-running", &short, 10_000);
    let (lines, _guard) = capture_targets();
    walk(&mut engine, &[], (true, &[booked]), NOW_MS);
    engine.heard(&host("a"), Some(&reading(1, 6 * GIB)), at(NOW_MS + 2_000));
    engine.heard(&host("a"), Some(&reading(2, 2 * GIB)), at(NOW_MS + 4_000));
    let seen = engine.memory_shadow_view().unwrap().hosts[&host("a")];
    let expected = HostMemoryShadow {
        projected_bytes: margined(2048),
        measured_bytes: Some(2 * GIB),
        checks: 2,
        too_low: 1,
        short_bytes: 6 * GIB - margined(2048),
    };
    assert_eq!(seen, expected);
    assert_eq!(
        shadow_lines(lines.taken()),
        [
            " INFO js_sched_placer::sizes::profile: placement curves: a host held more memory than its jobs' memory curves projected host=a projected_mib=2585 measured_mib=6144 ahead_ms=2000"
        ]
    );
    engine.heard(&host("a"), Some(&reading(3, 6 * GIB)), at(NOW_MS + 5_000));
    walk(&mut engine, &[], (true, &[booked]), NOW_MS + 6_000);
    let table = &engine.memory.tables[&host("a")];
    assert_eq!(table.residual, 6 * GIB - margined(2048));
}

/// Asserts the placement the curve would not have made: the host's last reading, 1 s old, shows
/// 15 GiB in use while the room books nothing, so the flat booking places a job of `link` and the
/// curve, with the reading carried to every slot, is over the 20 GiB at slot 20, where the job
/// links; it is counted and logged as later with that slot. A reading 16 s old is stale and
/// carried nowhere: the same placement then counts as the same. A reading heard before any walk
/// is kept but compared with nothing. Catches a residual left out of the fit and a stale reading
/// kept in it.
#[test]
fn a_placement_the_curve_would_not_have_made_is_counted_later_with_its_slot() {
    let mut views = Vec::new();
    for heard_ms in [NOW_MS - 1_000, NOW_MS - 16_000] {
        let tables = Tables::new();
        let mut engine = engine_in(Curves::Shadow);
        taught(&mut engine, &tables, 24);
        let link = queued_of(&tables, "link-new", "link");
        engine.heard(&host("a"), Some(&reading(1, 15 * GIB)), at(heard_ms));
        let (lines, _guard) = capture_targets();
        let placed = walk(&mut engine, &[link], (true, &[]), NOW_MS);
        assert_eq!(placed.len(), 1);
        let view = engine.memory_shadow_view().unwrap();
        assert_eq!(view.hosts[&host("a")].checks, 0);
        views.push(((view.same, view.later), shadow_lines(lines.taken())));
    }
    let later = " INFO js_sched_placer::sizes::profile: placement curves: a job's memory curve would not have placed it on its host job=link-new class=link host=a slot=20";
    assert_eq!(views[0], ((0, 1), vec![later.to_owned()]));
    assert_eq!(views[1], ((1, 0), Vec::new()));
}

/// Asserts what a host with no capacity is: the room does not hold it to a memory figure, so the
/// shadow has no table of it, a job placed there is counted in no verdict, and a reading of it
/// heard before the walk leaves the view once the walk has no table of it. Beside it, host `b` is
/// held to 1 GiB, too little for the job's flat booking, so the walk has a table (of `b`) and
/// judges the round, yet the job is placed on `a`. Catches a host projected against a capacity it
/// does not have, a placement there counted against another host's table, and a forgotten host
/// kept in the view.
#[test]
fn a_host_with_no_capacity_is_not_judged() {
    let tables = Tables::new();
    let mut engine = engine_in(Curves::Shadow);
    engine.host_up(&host("b"), &shape(), at(0));
    taught(&mut engine, &tables, 24);
    let link = queued_of(&tables, "link-new", "link");
    engine.heard(&host("a"), Some(&reading(1, GIB)), at(NOW_MS - 1_000));
    assert!(!engine.memory_shadow_view().unwrap().hosts.is_empty());
    let small = Capacity {
        cpu_millis: 88_000,
        memory_bytes: GIB,
        disk_bytes: 0,
        accelerators: 0,
    };
    let b = OpenHost {
        capacity: Some(small),
        ..OpenHost::default()
    };
    let mut open = open(false);
    open.insert(host("b"), b);
    engine.set_room(Room::of(&open, &[]));
    let (decided, _) = engine.walk_jobs(&[link], &open, at(NOW_MS));
    let placed: Vec<HostId> = decided.into_iter().map(|(_, on, _, _)| on).collect();
    assert_eq!(placed, vec![host("a")]);
    let view = engine.memory_shadow_view().unwrap();
    assert_eq!((view.same, view.sooner, view.later), (0, 0, 0));
    let hosts: Vec<&HostId> = view.hosts.keys().collect();
    assert_eq!(hosts, vec![&host("b")]);
}

/// Asserts which jobs are projected by their class: one that leaves its memory at 0, not one
/// that names it nor one that books the whole host; and that a history that keeps no profile
/// (`off`) or has not seen a class has no memory curve for it. Catches a named memory projected
/// by its class, and a curve read from nothing.
#[test]
fn only_a_job_that_leaves_its_memory_to_its_class_has_a_curve() {
    let tables = Tables::new();
    let shape = |cpu, memory| {
        let needs = js_jobs::Needs::Shape(Resources::new(cpu, memory, 0, None).unwrap());
        tables.submit_with(&format!("job-{cpu}-{memory}"), needs, 600)
    };
    assert!(leaves_memory(&shape(0, 0)));
    assert!(leaves_memory(&shape(1_000, 0)));
    assert!(!leaves_memory(&shape(0, MIB)));
    assert!(!leaves_memory(&tables.whole("whole")));
    let cold = ColdSize {
        cpu_millis: 2_000,
        memory_bytes: 4 * GIB,
    };
    let key = JobSizes::key_of(&queued_of(&tables, "unseen", "unseen"));
    for curves in [Curves::Off, Curves::Shadow] {
        let sizes = JobSizes::with_cold(cold).with_curves(curves);
        assert_eq!(sizes.memory_curve_of(&key, GIB, at(NOW_MS)), None);
    }
}
