//! Replays of runs shaped like a build farm's history through the sizer, and one pass of the
//! placer over a farm host: what the history books once a job's CPU booking is a share and not
//! a ceiling (`docs/design/sizing.md`, 7.9).
//!
//! The farm: hosts of 88 cores and 251 GiB. Its build jobs name no CPU or memory; each names
//! its target as `size-class` and its kind as `size-fallback`. Their peaks are 0.6 to 3.2 GB,
//! and before the change every one of them ran under a `cpu.max` of its booking (0.25 to 1.25
//! cores) and used all of it, booked at the cold memory (16 GiB). The kinds below are that
//! mix: per kind, a spread of peaks and the mean cores a run uses when nothing holds it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    Booked, CpuThrottle, JobEnd, JobId, JobRecord, JobSpec, Needs, NewJob, Output, ResourceUsage,
};
use js_types::{Labels, ManualClock, Resources, Shape, Timestamp};

use super::{GIB, Tables, at, booked, bounds_of, capacity, host, needs, pass, queued, ran};
use crate::engine::Settings;
use crate::engine::rig::spec_of;
use crate::place::SchedPlacer;
use crate::sizes::{JobSizes, SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL, SizedFrom};

/// The farm's host: 88 cores and 251 GiB.
const HOST: (u64, u64) = (88_000, 251 * GIB);

/// One kind of build step: its name, its least and largest peak, and the mean millicores a
/// run of it uses when nothing holds it.
struct Kind {
    name: &'static str,
    peaks: (u64, u64),
    cores: u64,
}

/// The farm's mix: compiles that keep many processes busy, tests, and small genrules.
const KINDS: [Kind; 3] = [
    Kind {
        name: "zig-cc",
        peaks: (600_000_000, 2_000_000_000),
        cores: 6000,
    },
    Kind {
        name: "cargo-test",
        peaks: (1_000_000_000, 3_200_000_000),
        cores: 2000,
    },
    Kind {
        name: "genrule",
        peaks: (600_000_000, 1_200_000_000),
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

/// A run's end: exit 0 after ten seconds that peaked at `peak` and used `cores` millicores on
/// average, as an agent reported it: `held` (throttled in every period, or `None` for an agent
/// from before the counters) and its CPU contention in thousandths (`None`: not reported).
fn ended(peak: u64, cores: u64, throttle: Option<CpuThrottle>, contention: Option<u64>) -> JobEnd {
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(ResourceUsage {
                cpu_time: Duration::from_millis(cores * 10),
                peak_memory_bytes: peak,
                cpu_throttle: throttle,
                cpu_pressure_some: contention.map(|permille| Duration::from_millis(permille * 10)),
            }),
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
/// and a quarter), reported by an agent with no throttle counters and no pressure.
fn capped_history(tables: &Tables) -> Vec<(JobRecord, Booked)> {
    let mut history = Vec::new();
    for kind in &KINDS {
        for target in 0..TARGETS {
            let class = format!("{}.t{target}", kind.name);
            for run in 0..=(target % 3) {
                let cap = 250 * (1 + u64::try_from((target + run) % 5).unwrap());
                let booked = booked(cap, 16 * GIB);
                let end = ended(peak(kind, target, run), cap * 99 / 100, None, None);
                let id = format!("{class}.r{run}");
                let spec = step_spec(&class, kind.name);
                history.push((ran(tables, (&id, None), spec, booked, end), booked));
            }
        }
    }
    history
}

/// Asserts, over the farm-shaped history above, what every target is booked next: at least
/// the largest peak it ever had (no target is booked below a peak of its own), from its kind's
/// peaks and not the cold size, at most twice the real peaks in sum (here 1.78 times),
/// and so 97 of its jobs fit a farm host by memory where 15 did at 16 GiB. Catches a young
/// target booked the cold memory (the 16 GiB that filled the farm's hosts at 15 jobs), a
/// fallback that drops a target's own larger peak, and a kind's estimate thrown away for
/// being spread (its largest peak over four times its median).
#[test]
fn the_farms_targets_book_their_kinds_peaks_not_the_cold_size_and_never_below_their_own() {
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
    assert_eq!(factor_hundredths, 178, "booked over peaks, in hundredths");
    assert_eq!(
        HOST.1 / (booked_sum / jobs),
        97,
        "jobs a host fits by memory"
    );
    assert_eq!(
        HOST.1 / (16 * GIB),
        15,
        "jobs a host fit at the old cold memory"
    );
}

/// Asserts the CPU of a class whose whole history ran held at a quarter core (a server's
/// first start after the change): the held runs teach it no CPU, so its first job books the
/// cold CPU (4 cores), not the 250 m the old history would have held it at; its first run
/// without a ceiling, on a busy host (contention 40%), books what it used (6 cores) at once,
/// since a run that waited may still raise; and quiet runs of 300 m bring it down to 500 m
/// after nine of them (the 90th percentile of ten samples is the 9th smallest). Catches capped
/// history kept as measurements (the class would stay at 250 m: the absorbing floor), a
/// contended run that cannot raise, and a booking that never comes down.
#[test]
fn a_class_held_at_a_quarter_core_rises_to_its_real_use_in_one_run_and_falls_on_quiet_runs() {
    let tables = Tables::new();
    let class = "zig-cc.held";
    let mut history = Vec::new();
    for run in 0..32 {
        let end = ended(GIB, 248, None, None);
        let id = format!("old-{run}");
        let spec = step_spec(class, "zig-cc.held-kind");
        history.push((
            ran(&tables, (&id, None), spec, booked(250, 16 * GIB), end),
            booked(250, 16 * GIB),
        ));
    }
    let next = queued("next", step_spec(class, "zig-cc.held-kind"));
    let sizes = farm_taught(&history);
    assert_eq!(sized(&sizes, &next).0, 4000, "the cold CPU, not 250 m");

    let busy = ended(GIB, 6000, UNCAPPED, Some(400));
    let spec = step_spec(class, "zig-cc.held-kind");
    history.push((
        ran(&tables, ("new-0", None), spec, booked(4000, 4 * GIB), busy),
        booked(4000, 4 * GIB),
    ));
    let sizes = farm_taught(&history);
    assert_eq!(sized(&sizes, &next).0, 6000, "after one uncapped run");

    let mut falls_after = None;
    for run in 1..=12 {
        let quiet = ended(GIB, 300, UNCAPPED, Some(20));
        let spec = step_spec(class, "zig-cc.held-kind");
        let id = format!("new-{run}");
        history.push((
            ran(&tables, (&id, None), spec, booked(6000, 4 * GIB), quiet),
            booked(6000, 4 * GIB),
        ));
        let cpu = sized(&farm_taught(&history), &next).0;
        if cpu == 500 && falls_after.is_none() {
            falls_after = Some(run);
        }
    }
    assert_eq!(falls_after, Some(9), "quiet runs before the booking falls");
}

/// Asserts the CPU of a class while agents still hold each job at its booking (a new server
/// beside agents from before the change): a run held at the cold CPU raises the class to it,
/// and further held runs, however many, never lower it, with the throttle counters or without
/// them. Catches a held run taken as a measurement, which brought the farm's classes down to a
/// quarter core and never let them up.
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
            let end = ended(GIB, 3980, throttle, None);
            let spec = step_spec(&class, &format!("{class}-kind"));
            let id = format!("{class}.r{run}");
            let booked = booked(4000, 4 * GIB);
            history.push((ran(&tables, (&id, None), spec, booked, end), booked));
        }
        let next = queued(
            &format!("{class}.next"),
            step_spec(&class, &format!("{class}-kind")),
        );
        assert_eq!(sized(&farm_taught(&history), &next).0, 4000, "{label}");
    }
}

/// Asserts the most a class's history books of a host's CPU: a class whose runs use all 88
/// cores of an idle host books an eighth of them (11 cores), so eight of its jobs leave a farm
/// host 8.8 cores of its 96.8 (88 at the default slack of 1.1) for other jobs, and its runs
/// do not book a host alone. Catches a learned CPU clamped only at the host's cores.
#[test]
fn a_greedy_class_books_at_most_an_eighth_of_a_host() {
    let tables = Tables::new();
    let mut history = Vec::new();
    for run in 0..5 {
        let end = ended(GIB, 88_000, UNCAPPED, Some(0));
        let spec = step_spec("link.all", "link");
        let booked = booked(4000, 4 * GIB);
        history.push((
            ran(&tables, (&format!("link-{run}"), None), spec, booked, end),
            booked,
        ));
    }
    let next = queued("next", step_spec("link.all", "link"));
    let (cpu, _, from) = sized(&farm_taught(&history), &next);
    assert_eq!((cpu, from), (11_000, SizedFrom::Class));
    assert_eq!(HOST.0 * 11 / 10 - 8 * cpu, 8800);
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

/// Asserts what one pass of the placer puts on one farm host once the history has seen each
/// kind run without a ceiling: from 240 queued steps of the mix (a link step that uses all 88
/// cores among every tenth), the host is charged to within one compile (6 cores) of its CPU
/// slack limit (96.8 cores; here 95.0) and within its memory, holds every kind, and holds more
/// jobs than the 15 the old cold memory let it (here 27).
/// Catches CPU bookings that leave the host short of full, one class's jobs taking the host
/// alone, and memory bookings that fill it first.
#[test]
fn a_farm_host_is_booked_full_by_the_mix_and_holds_every_kind() {
    let tables = Tables::new();
    let placer = farm_placer();
    let mut n = 0;
    for kind in &KINDS {
        for target in 0..8 {
            let class = format!("{}.seen{target}", kind.name);
            let end = ended(peak(kind, target, 0), kind.cores, UNCAPPED, Some(0));
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
    let end = ended(2 * GIB, 88_000, UNCAPPED, Some(0));
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
        let (class, kind) = if at % 10 == 9 {
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
        left > HOST.0 * 11 / 10 - 6000,
        "{left}: within one compile of the slack limit"
    );
    assert!(memory <= HOST.1, "{memory}");
    assert_eq!(kinds.len(), 4, "{kinds:?}");
    assert!(placed.len() > 15, "{}", placed.len());
}

/// Asserts the source and size of a young target when its kind gives no figure: a kind (and so
/// the client's whole history) whose runs reach the host is no fallback, so a target of it
/// whose own runs peaked at 6 GiB books its own peak with the margin (`class`, 7.5 GiB), one
/// whose runs peaked at 1 GiB books the cold memory (`cold`, 4 GiB), and the target with five
/// runs of 220 GiB on a 251 GiB host, whose estimate with the margin reaches the host, books
/// the whole host (`over-cap`). Under another client, a target of a kind that gives a figure
/// books the kind's (`fallback`). Catches a target booked below a peak of its own, and a settled
/// target past the host given a share.
#[test]
fn a_young_target_books_the_larger_of_its_own_peak_and_its_fallback_and_a_large_one_the_host() {
    let tables = Tables::new();
    let mut history = Vec::new();
    let mut run = |(id, client): (&str, Option<&str>), class: &str, peak: u64| {
        let spec = step_spec(class, "kind.mix");
        // Booked twice its peak, so that no run reads as having hit its limit.
        let booked = booked(1000, 2 * peak);
        let end = ended(peak, 1000, UNCAPPED, Some(0));
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
    assert_eq!((small.1, small.2), (4 * GIB, SizedFrom::Cold));
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
