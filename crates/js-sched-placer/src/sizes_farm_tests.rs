//! Replays of runs shaped like a build farm's history through the sizer, and one pass of the
//! placer over a farm host: what the history books once a job's CPU booking is a share and not
//! a ceiling (`docs/design/sizing.md`, 7.9).
//!
//! The farm: hosts of 88 cores and 251 GiB. Its build jobs name no CPU or memory; each names
//! its target as `size-class` and its kind as `size-fallback`. Their memory peaks are 0.6 to
//! 3.2 GB, and before the change every one of them ran under a `cpu.max` of its booking (0.25
//! to 1.25 cores) and used all of it, booked at the cold memory (16 GiB), reported by an agent
//! that measured no CPU peak. The kinds below are that mix: per kind, a spread of memory peaks,
//! the most cores a run of it uses over five seconds (what a new agent reports, kept in the
//! job's usage) and the mean cores it uses over its whole run, when nothing holds it (what a
//! class books).

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    Booked, Capacity, CpuPeak, CpuThrottle, JobEnd, JobId, JobRecord, JobSpec, Needs, NewJob,
    Output, ResourceUsage,
};
use js_registry::Placer as _;
use js_types::sched::usage::NodeLoad;
use js_types::{Labels, ManualClock, Resources, Shape, Timestamp};

use super::{
    GIB, Quiet, Tables, at, booked, bounds_of, capacity, host, needs, pass, queued, ran, ran_for,
};
use crate::engine::Settings;
use crate::engine::jobs::{JobHosts, OpenHost};
use crate::engine::rig::spec_of;
use crate::place::SchedPlacer;
use crate::sizes::{JobSizes, SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL, SizedFrom};

/// The farm's host: 88 cores and 251 GiB.
const HOST: (u64, u64) = (88_000, 251 * GIB);

/// One kind of build step: its name, its least and largest memory peak, the most millicores a
/// run of it uses over five seconds, and the mean millicores it uses over its whole run, when
/// nothing holds it.
struct Kind {
    name: &'static str,
    peaks: (u64, u64),
    cpu_peak: u64,
    cores: u64,
}

/// The farm's mix: compiles that burst to 20 cores, tests, and small genrules.
const KINDS: [Kind; 3] = [
    Kind {
        name: "zig-cc",
        peaks: (600_000_000, 2_000_000_000),
        cpu_peak: 20_000,
        cores: 6000,
    },
    Kind {
        name: "cargo-test",
        peaks: (1_000_000_000, 3_200_000_000),
        cpu_peak: 4000,
        cores: 2000,
    },
    Kind {
        name: "genrule",
        peaks: (600_000_000, 1_200_000_000),
        cpu_peak: 1000,
        cores: 500,
    },
];

/// Targets of each kind in the replay.
const TARGETS: usize = 40;

/// The peak of run `run` of target `target` of `kind`: spread over the kind's range by a
/// fixed stride, so each replay is the same.
fn peak(kind: &Kind, target: usize, run: usize) -> u64 {
    let (low, high) = kind.peaks;
    let step = (target * 37 + run * 11) % 100;
    low + (high - low) * u64::try_from(step).unwrap() / 99
}

/// The spec of a step of `kind` for `target` that books nothing.
fn step_spec(target: &str, kind: &str) -> JobSpec {
    let mut spec = spec_of(needs(0, 0), 600);
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), target.to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), kind.to_owned()),
    ]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    spec
}

/// A run's end: exit 0 after ten seconds that peaked at `peak` bytes of memory and used
/// `cores` millicores on average, as an agent reported it: `cpu_peak` (the most millicores over
/// five seconds, or `None` for an agent from before the peak), the throttle counters (`None`
/// for an agent from before them) and its CPU contention in thousandths (`None`: not reported).
fn ended(
    peak: u64,
    cores: u64,
    cpu_peak: Option<u64>,
    throttle: Option<CpuThrottle>,
    contention: Option<u64>,
) -> JobEnd {
    exit_with(ResourceUsage {
        cpu_time: Duration::from_millis(cores * 10),
        peak_memory_bytes: peak,
        cpu_throttle: throttle,
        cpu_pressure_some: contention.map(|permille| Duration::from_millis(permille * 10)),
        cpu_peak: cpu_peak.map(|millicores| CpuPeak {
            millicores,
            window_usec: 5_000_000,
        }),
    })
}

/// An exit with code 0 that reported `usage`.
fn exit_with(usage: ResourceUsage) -> JobEnd {
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(usage),
            ..Output::default()
        },
    }
}

/// An uncapped group: no `cpu.max`, so no periods.
const UNCAPPED: Option<CpuThrottle> = Some(CpuThrottle {
    periods: 0,
    throttled_periods: 0,
});

/// Sizes over one farm host with the default cold size, taught `jobs` with what each booked.
fn farm_taught(jobs: &[(JobRecord, Booked)]) -> JobSizes {
    let mut sizes = JobSizes::new();
    sizes.set_bounds(bounds_of([capacity(HOST.0, HOST.1)]));
    for (job, booked) in jobs {
        sizes.observe(job, *booked);
    }
    sizes
}

/// The CPU, memory and source `sizes` books for `job` on day 0.
fn sized(sizes: &JobSizes, job: &JobRecord) -> (u64, u64, SizedFrom) {
    match &sizes.sized(job, js_sizing::Day(0)).spec.needs {
        Needs::Shape(resources) => (
            resources.cpu_millis(),
            resources.memory_bytes(),
            sizes.sized_from(job, js_sizing::Day(0)).unwrap(),
        ),
        Needs::WholeHost => panic!("a share is sized as a share"),
    }
}

/// The farm's history before the change: every target of every kind ran one to three times,
/// booked the old cold memory (16 GiB) and a CPU ceiling it used all of (a quarter core to one
/// and a quarter), reported by an agent with no CPU peak, no throttle counters and no pressure:
/// each used 99% of its ceiling, so each is held.
fn capped_history(tables: &Tables) -> Vec<(JobRecord, Booked)> {
    let mut history = Vec::new();
    for kind in &KINDS {
        for target in 0..TARGETS {
            let class = format!("{}.t{target}", kind.name);
            for run in 0..=(target % 3) {
                let cap = 250 * (1 + u64::try_from((target + run) % 5).unwrap());
                let booked = booked(cap, 16 * GIB);
                let end = ended(peak(kind, target, run), cap * 99 / 100, None, None, None);
                let id = format!("{class}.r{run}");
                let spec = step_spec(&class, kind.name);
                history.push((ran(tables, (&id, None), spec, booked, end), booked));
            }
        }
    }
    history
}

/// Asserts, over the farm-shaped history above, what every target is booked next: at least
/// the largest peak it ever had (no target is booked below a peak of its own), from its own
/// peaks and not its kind's largest or the cold size, so the bookings sum to 1.37 times the
/// real peaks (1.78 when each young target booked its kind's largest) and 126 of its jobs fit a
/// farm host by memory (97 then, and 15 at 16 GiB). Catches a young target booked the cold
/// memory (the 16 GiB that filled the farm's hosts at 15 jobs) or held at its kind's largest
/// peak, and a target booked below a peak of its own.
#[test]
fn the_farms_targets_book_their_own_peaks_not_their_kinds_or_the_cold_size() {
    let tables = Tables::new();
    let history = capped_history(&tables);
    let sizes = farm_taught(&history);
    let (mut booked_sum, mut peak_sum, mut jobs) = (0u64, 0u64, 0u64);
    for kind in &KINDS {
        for target in 0..TARGETS {
            let class = format!("{}.t{target}", kind.name);
            let own = (0..=(target % 3))
                .map(|run| peak(kind, target, run))
                .max()
                .unwrap();
            let next = queued(&format!("{class}.next"), step_spec(&class, kind.name));
            let (_, memory, from) = sized(&sizes, &next);
            assert!(memory >= own, "{class}: {memory} below its own peak {own}");
            assert_ne!(from, SizedFrom::Cold, "{class}");
            assert!(memory <= 4 * GIB, "{class}: {memory}");
            booked_sum += memory;
            peak_sum += own;
            jobs += 1;
        }
    }
    let factor_hundredths = booked_sum * 100 / peak_sum;
    assert_eq!(factor_hundredths, 137, "booked over peaks, in hundredths");
    assert_eq!(
        HOST.1 / (booked_sum / jobs),
        126,
        "jobs a host fits by memory"
    );
    assert_eq!(
        HOST.1 / (16 * GIB),
        15,
        "jobs a host fit at the old cold memory"
    );
}

/// Asserts the CPU of a class whose whole history ran held at a quarter core (a server's
/// first start after the change): the 32 old runs used 248 m of their 250 m booking (within
/// 5%, with no throttle counters), so each counts as held and, at no more than the cold
/// estimate, teaches no CPU, and the class's first job books the cold CPU (22 cores on a farm host),
/// not the 250 m the old history would have held it at; its first run of a new agent, which
/// used 6 cores on average (peaking at 20) on a busy host (contention 40%), books its mean,
/// 6 cores, at once, since a run that waited may still raise; and quiet runs of 0.3 cores
/// bring it down to 500 m with the first of them (the median of two samples, 6 cores and 0.3,
/// is the smaller, the 1st smallest; 0.3 rounds up to 500 m). Catches capped history kept as
/// measurements (the class would stay at 250 m: the absorbing floor), a contended run that
/// cannot raise, and a booking that never comes down.
#[test]
fn a_class_held_at_a_quarter_core_rises_to_its_real_use_in_one_run_and_falls_on_quiet_runs() {
    let tables = Tables::new();
    let class = "zig-cc.held";
    let mut history = Vec::new();
    for run in 0..32 {
        let end = ended(GIB, 248, None, None, None);
        let id = format!("old-{run}");
        let spec = step_spec(class, "zig-cc.held-kind");
        history.push((
            ran(&tables, (&id, None), spec, booked(250, 16 * GIB), end),
            booked(250, 16 * GIB),
        ));
    }
    let next = queued("next", step_spec(class, "zig-cc.held-kind"));
    let sizes = farm_taught(&history);
    assert_eq!(sized(&sizes, &next).0, 22_000, "the cold CPU, not 250 m");

    let busy = ended(GIB, 6000, Some(20_000), UNCAPPED, Some(400));
    let spec = step_spec(class, "zig-cc.held-kind");
    history.push((
        ran(
            &tables,
            ("new-0", None),
            spec,
            booked(22_000, 4 * GIB),
            busy,
        ),
        booked(22_000, 4 * GIB),
    ));
    let sizes = farm_taught(&history);
    assert_eq!(sized(&sizes, &next).0, 6000, "after one new run: its mean");

    let mut falls_after = None;
    for run in 1..=12 {
        let quiet = ended(GIB, 300, Some(300), UNCAPPED, Some(20));
        let spec = step_spec(class, "zig-cc.held-kind");
        let id = format!("new-{run}");
        history.push((
            ran(&tables, (&id, None), spec, booked(20_000, 4 * GIB), quiet),
            booked(20_000, 4 * GIB),
        ));
        let cpu = sized(&farm_taught(&history), &next).0;
        if cpu == 500 && falls_after.is_none() {
            falls_after = Some(run);
        }
    }
    assert_eq!(falls_after, Some(1), "quiet runs before the booking falls");
}

/// Asserts the CPU of a class while its jobs are still held at their booking (a new server
/// beside old agents): runs held at the cold CPU (22 cores), whose mean CPU is a lower bound,
/// keep the class at it, however many, and never lower it, with the throttle counters or
/// without them. Catches a held run taken as a measurement, which brought the farm's classes
/// down to a quarter core and never let them up.
#[test]
fn runs_still_held_at_their_booking_never_lower_a_class() {
    let tables = Tables::new();
    let throttled = Some(CpuThrottle {
        periods: 1000,
        throttled_periods: 990,
    });
    for (label, throttle) in [("no-counters", None), ("counters", throttled)] {
        let class = format!("held.{label}");
        let mut history = Vec::new();
        for run in 0..40 {
            let end = ended(GIB, 21_780, None, throttle, None);
            let spec = step_spec(&class, &format!("{class}-kind"));
            let id = format!("{class}.r{run}");
            let booked = booked(22_000, 4 * GIB);
            history.push((ran(&tables, (&id, None), spec, booked, end), booked));
        }
        let next = queued(
            &format!("{class}.next"),
            step_spec(&class, &format!("{class}-kind")),
        );
        assert_eq!(sized(&farm_taught(&history), &next).0, 22_000, "{label}");
    }
}

/// Asserts the most a class's history books of a host's CPU: a class whose runs use all 88
/// cores of an idle host on average books a quarter of them (22 cores), so four of its jobs leave a farm
/// host 8.8 cores of its 96.8 (88 at the default slack of 1.1) for other jobs, and its runs
/// do not book a host alone. Catches a learned CPU clamped only at the host's cores.
#[test]
fn a_greedy_class_books_at_most_a_quarter_of_a_host() {
    let tables = Tables::new();
    let mut history = Vec::new();
    for run in 0..5 {
        let end = ended(GIB, 88_000, Some(88_000), UNCAPPED, Some(0));
        let spec = step_spec("link.all", "link");
        let booked = booked(22_000, 4 * GIB);
        history.push((
            ran(&tables, (&format!("link-{run}"), None), spec, booked, end),
            booked,
        ));
    }
    let next = queued("next", step_spec("link.all", "link"));
    let (cpu, _, from) = sized(&farm_taught(&history), &next);
    assert_eq!((cpu, from), (22_000, SizedFrom::Class));
    assert_eq!(HOST.0 * 11 / 10 - 4 * cpu, 8800);
}

/// A placer over one farm host `a` with the default cold size, on a clock at zero.
fn farm_placer() -> SchedPlacer {
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let resources = Resources::new(HOST.0, HOST.1, 0, None).unwrap();
    placer
        .engine()
        .host_up(&host("a"), &Shape::new(resources, caps), at(0));
    placer
}

/// Asserts what one pass of the placer puts on one farm host, with no load reading yet, once
/// the history has seen each kind run with a new agent: from 240 queued steps of the mix (a
/// link step that uses 30 cores, booked a quarter of the host, first among every ten), each
/// class books its runs' mean CPU, but a compile, whose runs average 6 cores and burst to 20
/// (a heavy tail: over twice its mean), books its peak, and the host is charged to within one
/// compile's mean (6 cores) of its CPU slack limit (96.8 cores), within its memory, and holds
/// every kind. Worked by hand from the bookings: 10 jobs are placed (a link step of 22 cores,
/// then three of each test of 2, genrule of 0.5 and compile of 20), booked 89.5 cores and
/// charged 91.0 (a host with no reading charges a lease at least a core, so each genrule
/// counts 1; the next job of the queue, a link step, needs 22 and waits for the room, holding
/// the host), and 26.3 GB of memory. Catches CPU bookings that leave the host short of full,
/// a compile booked at its mean that bursts the host past its cores, one class's jobs taking
/// the host alone, and a kind that never gets a place.
#[test]
fn a_farm_host_is_booked_full_by_the_mix_and_holds_every_kind() {
    let tables = Tables::new();
    let placer = farm_placer();
    let mut n = 0;
    for kind in &KINDS {
        for target in 0..8 {
            let class = format!("{}.seen{target}", kind.name);
            let end = ended(
                peak(kind, target, 0),
                kind.cores,
                Some(kind.cpu_peak),
                UNCAPPED,
                Some(0),
            );
            let booked = booked(1000, 4 * GIB);
            ran(
                &tables,
                (&format!("seen-{n}"), None),
                step_spec(&class, kind.name),
                booked,
                end,
            );
            n += 1;
        }
    }
    let end = ended(2 * GIB, 30_000, Some(88_000), UNCAPPED, Some(0));
    ran(
        &tables,
        ("seen-link", None),
        step_spec("link.all", "link"),
        booked(1000, 4 * GIB),
        end,
    );
    placer.learn_ended(&tables.jobs).unwrap();

    let mut queued_kinds = BTreeMap::new();
    for at in 0..240 {
        let (class, kind) = if at % 10 == 0 {
            (format!("link.t{at}"), "link")
        } else {
            let kind = KINDS[at % 3].name;
            (format!("{kind}.q{at}"), kind)
        };
        let id = format!("q-{at:03}");
        let new = NewJob {
            id: Some(JobId::parse(&id).unwrap()),
            spec: step_spec(&class, kind),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
        queued_kinds.insert(id, kind);
    }
    let placed = pass(&placer, &tables);
    let (mut cpu, mut memory) = (0, 0);
    let mut kinds = std::collections::BTreeSet::new();
    for id in &placed {
        let Booked::Shape {
            cpu_millis,
            memory_bytes,
            ..
        } = tables.jobs.get_allocation(id).unwrap().unwrap().booked
        else {
            panic!("a share")
        };
        cpu += cpu_millis;
        memory += memory_bytes;
        kinds.insert(queued_kinds[id.as_str()]);
    }
    let left = placer.engine().load_of("a").unwrap().left_cpu_millis;
    assert!(left <= HOST.0 * 11 / 10, "{left}");
    assert!(cpu <= left, "{cpu} booked, {left} charged");
    assert!(
        left > HOST.0 * 11 / 10 - KINDS[0].cores,
        "{left}: within one compile of the slack limit"
    );
    assert!(memory <= HOST.1, "{memory}");
    assert_eq!(kinds.len(), 4, "{kinds:?}");
    assert_eq!(
        (placed.len(), cpu, memory),
        (10, 89_500, 26_306_674_688),
        "jobs placed, CPU and memory booked"
    );
}

/// Two load readings of the farm host `a` as its agent sends them, `busy` millicores in use
/// between them, the first at `from` ms on the placer's `clock` and the second five seconds
/// later; `seq` numbers the first, and `used` is the CPU time its agent had counted by then,
/// in microseconds: the CPU time counted at the second.
fn measure(
    placer: &SchedPlacer,
    clock: &ManualClock,
    (seq, from, used): (u64, u64, u64),
    busy: u64,
) -> u64 {
    let reading = |seq: u64, used: u64| NodeLoad {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: used,
        ..NodeLoad::default()
    };
    clock.set(Timestamp::from_millis(from));
    placer.heard(&host("a"), Some(&reading(seq, used)));
    let after = used + busy * 5000;
    clock.set(Timestamp::from_millis(from + 5000));
    placer.heard(&host("a"), Some(&reading(seq + 1, after)));
    after
}

/// One pass of `placer` over `tables` with the farm host `a` open as a host whose agent reports
/// load admission, held to its slacked capacity (96.8 cores and its memory) as the server
/// holds such a host: the jobs it placed.
fn pass_by_load(placer: &SchedPlacer, tables: &Tables) -> Vec<JobId> {
    let offer = OpenHost {
        load_admission: true,
        capacity: Some(Capacity {
            cpu_millis: HOST.0 * 11 / 10,
            memory_bytes: HOST.1,
            disk_bytes: 0,
            accelerators: 0,
        }),
        ..OpenHost::default()
    };
    let open = BTreeMap::from([(host("a"), offer)]);
    let hello = std::collections::BTreeSet::new();
    let hosts = JobHosts {
        open: &open,
        hello: &hello,
        whole: None,
    };
    placer
        .place_jobs_with(&tables.jobs, &hosts, (&Quiet, &Quiet))
        .unwrap()
}

/// Asserts the farm's case: a compile whose one run, booked the cold 22 cores, used 2 cores on
/// average over 30 seconds and peaked at 20 over five (a new agent's report: no throttled
/// period, 1% CPU pressure) keeps its peak in its usage, and its class books its mean, 2
/// cores. A farm host (88 cores) whose agent reports load admission, measured at 10 cores
/// busy, then takes all twelve such jobs queued in one pass, where a booking of the peak took
/// four (4 x 20 of the 96.8-core slack limit); and once it is measured at 95 cores busy,
/// another compile waits although the bookings on it sum to 24 cores: the slack bounds the
/// measured load. Catches a class booked its peak, which held each farm host to four Mojo
/// compiles while it used a tenth of its cores.
#[test]
fn a_compile_that_peaks_at_20_cores_books_its_mean_and_a_host_with_measured_room_takes_twelve() {
    let tables = Tables::new();
    let class = "zig-cc.compile";
    let spec = || step_spec(class, "zig-cc");
    let end = exit_with(ResourceUsage {
        cpu_time: Duration::from_secs(60),
        peak_memory_bytes: 2 * GIB,
        cpu_throttle: UNCAPPED,
        cpu_pressure_some: Some(Duration::from_millis(300)),
        cpu_peak: Some(CpuPeak {
            millicores: 20_000,
            window_usec: 5_000_000,
        }),
    });
    let cold = booked(22_000, 4 * GIB);
    let run = ran_for(
        &tables,
        ("compile-0", None),
        spec(),
        cold,
        end,
        Duration::from_secs(30),
    );
    let usage = run.result.as_ref().unwrap().usage.unwrap();
    assert_eq!(
        usage.cpu_peak.unwrap().millicores,
        20_000,
        "the peak is kept"
    );

    let sizes = farm_taught(&[(run, cold)]);
    let next = queued("compile-next", spec());
    assert_eq!(sized(&sizes, &next).0, 2000, "the next job's CPU: the mean");

    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock.clone());
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let resources = Resources::new(HOST.0, HOST.1, 0, None).unwrap();
    placer
        .engine()
        .host_up(&host("a"), &Shape::new(resources, caps), at(0));
    placer.learn_ended(&tables.jobs).unwrap();
    let queue = |name: &str| {
        let new = NewJob {
            id: Some(JobId::parse(name).unwrap()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
    };
    for n in 0..12 {
        queue(&format!("queued-{n:02}"));
    }
    let used = measure(&placer, &clock, (1, 5000, 0), 10_000);
    let placed = pass_by_load(&placer, &tables);
    assert_eq!(placed.len(), 12, "every one: a peak booking placed four");
    for id in &placed {
        let Booked::Shape { cpu_millis, .. } =
            tables.jobs.get_allocation(id).unwrap().unwrap().booked
        else {
            panic!("a share")
        };
        assert_eq!(cpu_millis, 2000);
    }

    measure(&placer, &clock, (3, 15_000, used), 95_000);
    queue("late");
    assert!(pass_by_load(&placer, &tables).is_empty(), "95 + 2 > 96.8");
    let late = tables.jobs.get_job(&JobId::parse("late").unwrap()).unwrap();
    assert_eq!(late.unwrap().state, js_jobs::JobState::Queued);
}

/// Asserts the source and size of a young target when its kind gives no figure: a kind (and so
/// the client's whole history) whose runs reach the host is no fallback, so a target of it
/// whose own runs peaked at 6 GiB books its own peak with the margin (`class`, 7.5 GiB), one
/// whose runs peaked at 1 GiB books its own too (`class`, 1.5 GiB), and the target with five
/// runs of 220 GiB on a 251 GiB host, whose estimate with the margin reaches the host, books
/// the whole host (`over-cap`). Under another client, a target of a kind that gives a figure
/// books the kind's (`fallback`). Catches a target booked below a peak of its own, and a settled
/// target past the host given a share.
#[test]
fn a_young_target_books_its_own_peak_whatever_its_kind_and_a_large_one_the_host() {
    let tables = Tables::new();
    let mut history = Vec::new();
    let mut run = |(id, client): (&str, Option<&str>), class: &str, peak: u64| {
        let spec = step_spec(class, "kind.mix");
        // Booked twice its peak, so that no run reads as having hit its limit.
        let booked = booked(1000, 2 * peak);
        let end = ended(peak, 1000, Some(1000), UNCAPPED, Some(0));
        history.push((ran(&tables, (id, client), spec, booked, end), booked));
    };
    run(("k1", None), "kind.mix.big", 6 * GIB);
    run(("k2", None), "kind.mix.small", GIB);
    for n in 0..5 {
        run((&format!("h{n}"), None), "kind.mix.huge", 220 * GIB);
    }
    run(("o1", Some("other")), "kind.mix.small", GIB);
    let sizes = farm_taught(&history);
    let next = |class: &str| queued(&format!("{class}.next"), step_spec(class, "kind.mix"));
    let big = sized(&sizes, &next("kind.mix.big"));
    assert_eq!((big.1, big.2), (30 * 256 * (1 << 20), SizedFrom::Class));
    let small = sized(&sizes, &next("kind.mix.small"));
    assert_eq!((small.1, small.2), (6 * 256 * (1 << 20), SizedFrom::Class));
    let huge = sized(&sizes, &next("kind.mix.huge"));
    assert_eq!((huge.1, huge.2), (HOST.1, SizedFrom::OverCap));
    let other = queued_for("other", "kind.mix.fresh");
    assert_eq!(sized(&sizes, &other).2, SizedFrom::Fallback);
}

/// A queued step of `class` of the kind `kind.mix`, submitted by `client`.
fn queued_for(client: &str, class: &str) -> JobRecord {
    let tables = Tables::new();
    let new = NewJob {
        id: Some(JobId::parse(&format!("{class}.next")).unwrap()),
        spec: step_spec(class, "kind.mix"),
        priority: 0,
        client: Some(js_jobs::ClientName::parse(client).unwrap()),
    };
    tables.jobs.insert_queued(new).unwrap().record
}
