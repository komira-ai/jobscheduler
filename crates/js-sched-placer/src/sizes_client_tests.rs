//! The history of a client ([`JobSizes::client_key_of`]) and of a run's CPU: what a class books
//! from the runs of its client's other classes, what keeps a run's CPU from lowering its
//! class's, and the most CPU a class books.

use std::time::Duration;

use js_jobs::{CpuPeak, CpuThrottle, JobEnd, Output, ResourceUsage};
use js_sizing::Day;

use super::super::SizedFrom;
use super::{
    GIB, JobSizes, MIB, MODULE_COLD, Tables, allocation, booked, bounds, bounds_of, capacity,
    exited, finish, needs, of_class, pass, placer, queued, ran, runs, size, submit, submit_by,
};

/// An exit with code 0 after `cpu` of CPU time and a 1 GiB peak, with the CPU `throttle` and
/// `pressure` its host reported, from a new agent: its CPU peak is the run's mean over the ten
/// seconds [`ran`] gives each run (the peak is reported and kept, and the class books the
/// mean).
fn reported(cpu: Duration, throttle: Option<CpuThrottle>, pressure: Option<Duration>) -> JobEnd {
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage: Some(ResourceUsage {
                cpu_time: cpu,
                peak_memory_bytes: GIB,
                cpu_throttle: throttle,
                cpu_pressure_some: pressure,
                cpu_peak: Some(CpuPeak {
                    millicores: u64::try_from(cpu.as_millis() / 10).unwrap(),
                    window_usec: 5_000_000,
                }),
            }),
            ..Output::default()
        },
    }
}

/// The CPU the class `c` books after a run that used one core in its ten seconds (measured:
/// no pressure, no throttled period) and then ten runs ending as `then` gives, each booked
/// `then_booked` millicores: ten, so that a measurement lower than the core is the class's
/// median (the 6th smallest of eleven) and a lower bound is not.
fn cpu_after(then: impl Fn() -> JobEnd, then_booked: u64) -> u64 {
    let tables = Tables::new();
    let measured = reported(
        Duration::from_secs(10),
        Some(CpuThrottle {
            periods: 0,
            throttled_periods: 0,
        }),
        Some(Duration::ZERO),
    );
    let first = ran(
        &tables,
        ("first", None),
        of_class("c", needs(0, 0)),
        booked(4000, 4 * GIB),
        measured,
    );
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    assert!(sizes.observe(&first, booked(4000, 4 * GIB)));
    for n in 0..10 {
        let id = format!("then-{n}");
        let spec = of_class("c", needs(0, 0));
        let run = ran(
            &tables,
            (&id, None),
            spec,
            booked(then_booked, 4 * GIB),
            then(),
        );
        assert!(sizes.observe(&run, booked(then_booked, 4 * GIB)));
    }
    size(&sizes, &queued("next", of_class("c", needs(0, 0)))).0
}

/// Asserts that a run's CPU is a measurement only when it waited for a CPU less than a tenth
/// of its wall time and was not held at a ceiling, and may otherwise only raise its class's
/// CPU (a core, from the first run): after ten runs that used half a core, a class whose runs
/// were measured (pressure reported low, or not reported at all, as every run before the field,
/// with no throttled period) books half a core; one whose runs waited 20% of their wall time,
/// were throttled in one period in 20, or (with no throttle counters) came within 5% of their
/// booked CPU stays at a core; and ten such runs that used two cores raise it to two. Catches a
/// starved or held run lowering its class's CPU, one that raises it being dropped, and a run of
/// an agent from before the pressure field taken as starved (its class could never come down).
#[test]
fn a_run_under_contention_or_held_at_a_ceiling_only_raises_the_class_cpu() {
    let none = Some(CpuThrottle {
        periods: 0,
        throttled_periods: 0,
    });
    let one_in_twenty = Some(CpuThrottle {
        periods: 20,
        throttled_periods: 1,
    });
    let under = Some(CpuThrottle {
        periods: 21,
        throttled_periods: 1,
    });
    let quiet = Some(Duration::ZERO);
    let waited = Some(Duration::from_secs(2));
    let just_under = Some(Duration::from_millis(999));
    let secs = Duration::from_secs;
    let cases = [
        ("measured", none, quiet, 4000, 500),
        ("no pressure reported", none, None, 4000, 500),
        ("waited just under a tenth", none, just_under, 4000, 500),
        ("throttled in under 1 period in 20", under, quiet, 4000, 500),
        ("no counters, under the ceiling", None, quiet, 600, 500),
        ("waited a fifth", none, waited, 4000, 1000),
        (
            "throttled in 1 period in 20",
            one_in_twenty,
            quiet,
            4000,
            1000,
        ),
        ("no counters, at the ceiling", None, quiet, 500, 1000),
    ];
    for (why, throttle, pressure, booked_cpu, expected) in cases {
        let low = || reported(secs(5), throttle, pressure);
        assert_eq!(cpu_after(low, booked_cpu), expected, "{why}: half a core");
        let high = || reported(secs(20), throttle, pressure);
        assert_eq!(
            cpu_after(high, booked_cpu.max(2000)),
            2000,
            "{why}: two cores"
        );
    }
}

/// Asserts that a class books the mean CPU of its runs (CPU time over wall time), the median of
/// them, unless their five-second peaks are a heavy tail: five runs that used a core over their
/// ten seconds book a core when their host reported a peak of two cores (twice the mean, no
/// tail) or no peak at all (an agent from before it), and six cores, their peak, when it
/// reported six. A run with no peak is not taken as held. Catches a class whose runs burst six
/// times over its booking still booked at its mean, a class booked at a peak only twice its
/// mean, and a run with no peak reported that teaches its class nothing.
#[test]
fn a_class_books_the_mean_cpu_of_its_runs_unless_their_peaks_are_a_heavy_tail() {
    let tables = Tables::new();
    for (class, peak, books) in [
        ("peaked", Some(6000), 6000),
        ("steady", Some(2000), 1000),
        ("no-peak", None, 1000),
    ] {
        let end = || JobEnd::Exited {
            exit_code: 0,
            output: Output {
                usage: Some(ResourceUsage {
                    cpu_time: Duration::from_secs(10),
                    peak_memory_bytes: GIB,
                    cpu_throttle: Some(CpuThrottle {
                        periods: 0,
                        throttled_periods: 0,
                    }),
                    cpu_pressure_some: Some(Duration::ZERO),
                    cpu_peak: peak.map(|millicores| CpuPeak {
                        millicores,
                        window_usec: 5_000_000,
                    }),
                }),
                ..Output::default()
            },
        };
        let jobs = runs(&tables, class, 5, class, booked(4000, 4 * GIB), end);
        let mut sizes = JobSizes::with_cold(MODULE_COLD);
        sizes.set_bounds(Some(bounds()));
        for job in &jobs {
            assert!(sizes.observe(job, booked(4000, 4 * GIB)), "{}", job.id);
        }
        let next = queued("next", of_class(class, needs(0, 0)));
        assert_eq!(size(&sizes, &next).0, books, "{class}");
    }
}

/// Asserts that a class books at most a quarter of the cores of the largest host (rounded down
/// to a quarter core) and at least a quarter core: a run that used six cores on average books
/// two on a host of eight cores, four on one of sixteen, six on one of 32 and half a core on
/// one of two, and a run that used a tenth of a core books a quarter core. Catches a class whose
/// history books a host's CPU alone, and one booked no CPU.
#[test]
fn the_learned_cpu_is_at_most_a_quarter_of_the_largest_hosts_cores_and_at_least_a_quarter_core() {
    let tables = Tables::new();
    let end = |cpu: Duration| move || exited(0, GIB, cpu);
    let heavy = runs(
        &tables,
        "heavy",
        1,
        "heavy",
        booked(2000, 4 * GIB),
        end(Duration::from_secs(60)),
    );
    let light = runs(
        &tables,
        "light",
        1,
        "light",
        booked(2000, 4 * GIB),
        end(Duration::from_secs(1)),
    );
    for (cores, heavy_cpu) in [(8000, 2000), (16_000, 4000), (32_000, 6000), (2000, 500)] {
        let mut sizes = JobSizes::with_cold(MODULE_COLD);
        sizes.set_bounds(bounds_of([capacity(cores, 16 * GIB)]));
        assert!(sizes.observe(&heavy[0], booked(2000, 4 * GIB)));
        let next = queued("next", of_class("heavy", needs(0, 0)));
        assert_eq!(size(&sizes, &next).0, heavy_cpu, "{cores} millicores");
    }
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    assert!(sizes.observe(&light[0], booked(2000, 4 * GIB)));
    let next = queued("next", of_class("light", needs(0, 0)));
    assert_eq!(size(&sizes, &next).0, 250);
}

/// Asserts that jobs labelled with the class of one build target
/// ([`js_types::SizeClass::of_target`]) share that target's history across passes, that a job
/// of another target of the same client in the same pass is booked from that client's history
/// (the runs every target of the client taught), and that one of another client, which has no
/// history, is booked cold. Catches a repeat run of a target that is not booked from its own
/// runs, and a target's runs that teach another client.
#[test]
fn a_repeat_run_of_a_build_target_is_booked_from_its_runs_and_a_new_target_from_its_client() {
    let tables = Tables::new();
    let placer = placer();
    let class = |target: &str| {
        js_types::SizeClass::of_target(target)
            .unwrap()
            .as_str()
            .to_owned()
    };
    let built = class("root//crates/js-sched:js-sched");
    for run in 0..5 {
        let id = format!("js-sched-{run}");
        submit(&tables, &id, &built);
        pass(&placer, &tables);
        finish(&tables, &id);
    }
    submit(&tables, "js-sched-again", &built);
    submit(
        &tables,
        "js-state-first",
        &class("root//crates/js-state:js-state"),
    );
    submit_by(
        &tables,
        "js-state-other-client",
        &class("root//crates/js-state:js-state"),
        Some("other"),
    );
    pass(&placer, &tables);
    assert_eq!(
        allocation(&tables, "js-sched-again"),
        booked(3000, 11 * 256 * MIB),
        "the target's own runs"
    );
    assert_eq!(
        allocation(&tables, "js-state-first"),
        booked(3000, 11 * 256 * MIB),
        "another target: its client's history"
    );
    assert_eq!(
        allocation(&tables, "js-state-other-client"),
        booked(2000, 4 * GIB),
        "another client: cold"
    );
}

/// Asserts that only a run that carries its usage teaches a class: five successful runs with a
/// peak size the class from its own history (booked below the cold size, `sized_from` class)
/// and each answers `true`; five successful runs of another class whose ended jobs carry no
/// usage are seen, answer `false` and, taught on their own, leave that class cold at the cold
/// size (beside the measured runs it books from their client's history instead, `sized_from`
/// fallback). Catches a run with no measurement counted as taught (the count that said a whole
/// history was learned when none of it was) and a class that stays cold for lack of a usage
/// nobody sent.
#[test]
fn only_runs_with_usage_teach_a_class_and_the_rest_leave_it_cold() {
    let tables = Tables::new();
    let measured = || exited(0, 2 * GIB, Duration::from_secs(30));
    let unmeasured = || JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    let with = runs(
        &tables,
        "with",
        5,
        "measured",
        booked(2000, 4 * GIB),
        measured,
    );
    let without = runs(
        &tables,
        "without",
        5,
        "bare",
        booked(2000, 4 * GIB),
        unmeasured,
    );
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    for job in &with {
        assert!(sizes.observe(job, booked(2000, 4 * GIB)), "{}", job.id);
    }
    for job in &without {
        assert!(!sizes.observe(job, booked(2000, 4 * GIB)), "{}", job.id);
    }
    let next = |class: &str| queued(&format!("next-{class}"), of_class(class, needs(0, 0)));
    let (heard, silent) = (next("measured"), next("bare"));
    assert_eq!(sizes.sized_from(&heard, Day(0)), Some(SizedFrom::Class));
    assert_eq!(size(&sizes, &heard).1, 11 * 256 * MIB);
    // The measured runs taught their client's history as well, so the other class of that
    // client books from it; kept apart, the unmeasured runs leave their class cold.
    assert_eq!(sizes.sized_from(&silent, Day(0)), Some(SizedFrom::Fallback));
    let mut bare = JobSizes::with_cold(MODULE_COLD);
    bare.set_bounds(Some(bounds()));
    for job in &without {
        assert!(!bare.observe(job, booked(2000, 4 * GIB)), "{}", job.id);
    }
    assert_eq!(bare.sized_from(&silent, Day(0)), Some(SizedFrom::Cold));
    assert_eq!(size(&bare, &silent), (2000, 4 * GIB));
}
