//! A class's memory booking follows what its own runs used: from its first measured run it
//! books its own largest recent peak with the margin (`1.2 x` plus 128 MiB, at least 1 GiB),
//! not the largest peak of its kind or client nor the cold size, and a run that used more than
//! its booking raises the next booking at once. The host is a build farm's (88 cores and
//! 251 GiB) and the cold memory the 16 GiB a farm may still set, so a booking at the cold size
//! is told apart from every other.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{CpuPeak, JobEnd, JobRecord, JobSpec, Output, ResourceUsage};
use js_types::Labels;

use super::super::{ColdSize, SIZE_FALLBACK_LABEL, SizedFrom};
use super::{
    GIB, JobSizes, MIB, SIZE_CLASS_LABEL, Tables, booked, bounds_of, capacity, needs, output,
    queued, queued_by, ran_for, spec_of,
};

/// The farm's host: 88 cores and 251 GiB.
const HOST: (u64, u64) = (88_000, 251 * GIB);

/// The cold size of these tests: 22 cores and 16 GiB.
const COLD: ColdSize = ColdSize {
    cpu_millis: 22_000,
    memory_bytes: 16 * GIB,
};

/// The kind of every step here.
const KIND: &str = "env.bash.build.kind";

/// The spec of a step of the target `class`, of [`KIND`], that books nothing.
fn step_spec(class: &str) -> JobSpec {
    let mut spec = spec_of(needs(0, 0), 600);
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), class.to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), KIND.to_owned()),
    ]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    spec
}

/// `n` runs of the target `class`, each booked 4 cores and `booking` bytes, peaking at
/// `peak(run)` bytes over `wall` and using a core on average; ids `<class>-<prefix><n>`.
fn target_runs(
    tables: &Tables,
    (class, prefix): (&str, &str),
    n: usize,
    (booking, wall): (u64, Duration),
    peak: impl Fn(usize) -> u64,
) -> Vec<(JobRecord, js_jobs::Booked)> {
    (0..n)
        .map(|run| {
            let id = format!("{class}-{prefix}{run}");
            let booked = booked(4000, booking);
            let end = one_core(peak(run), wall);
            let job = ran_for(tables, (&id, None), step_spec(class), booked, end, wall);
            (job, booked)
        })
        .collect()
}

/// An exit with code 0 after `wall` that peaked at `peak` bytes and used a core, on average
/// and at its busiest (its five-second CPU peak is a core too).
fn one_core(peak: u64, wall: Duration) -> JobEnd {
    let usage = output(peak, wall).usage.map(|usage| ResourceUsage {
        cpu_peak: Some(CpuPeak {
            millicores: 1000,
            window_usec: 5_000_000,
        }),
        ..usage
    });
    JobEnd::Exited {
        exit_code: 0,
        output: Output {
            usage,
            ..Output::default()
        },
    }
}

/// Sizes over one farm host with the cold size [`COLD`], taught `runs` with what each booked.
fn farm_taught(runs: &[(JobRecord, js_jobs::Booked)]) -> JobSizes {
    let mut sizes = JobSizes::with_cold(COLD);
    sizes.set_bounds(bounds_of([capacity(HOST.0, HOST.1)]));
    for (job, booked) in runs {
        assert!(sizes.observe(job, *booked), "{} teaches a run", job.id);
    }
    sizes
}

/// The CPU, memory and source `sizes` books for the next step of `class` on day 0.
fn next(sizes: &JobSizes, class: &str) -> (u64, u64, SizedFrom) {
    let job = queued(&format!("{class}-next"), step_spec(class));
    let (cpu, memory) = super::size(sizes, &job);
    (
        cpu,
        memory,
        sizes.sized_from(&job, js_sizing::Day(0)).unwrap(),
    )
}

/// A large target of [`KIND`] (a compile that peaks at 9.3 GB) run three times at the cold
/// size: what holds a young target of the kind at its peak when the kind's largest sizes it.
fn large_target(tables: &Tables) -> Vec<(JobRecord, js_jobs::Booked)> {
    let wall = Duration::from_secs(300);
    target_runs(tables, ("big.build", "r"), 3, (16 * GIB, wall), |_| {
        9_340_407_808
    })
}

/// Asserts that a class of tiny jobs (a link step that peaks at 10 MB) books its own peak with
/// the margin, the least booking (1 GiB, `class`), from its first run on, beside a large
/// target of its kind that books 10.75 GiB: a run under a second as much as one of ten, and
/// with five or more runs whose peaks are spread (10 MB and 50 MB, over four times apart).
/// A run under a second gives no CPU sample, so such a class books its kind's CPU. Catches a
/// young class booked its kind's largest peak (10.75 GiB), a run under a second taught nothing
/// (the class young for ever), and a class with spread peaks booked the cold 16 GiB.
#[test]
fn a_class_of_tiny_jobs_books_near_its_peak_from_its_first_run_not_its_kind_or_the_cold_size() {
    let tables = Tables::new();
    let mut runs = large_target(&tables);
    let sizes = farm_taught(&runs);
    assert_eq!(
        next(&sizes, "big.build"),
        (1000, 43 * 256 * MIB, SizedFrom::Class)
    );
    assert_eq!(
        next(&sizes, "link.build"),
        (1000, 43 * 256 * MIB, SizedFrom::Fallback),
        "never seen: its kind's largest"
    );
    let brief = Duration::from_millis(400);
    runs.extend(target_runs(
        &tables,
        ("link.build", "brief"),
        1,
        (16 * GIB, brief),
        |_| 10_000_000,
    ));
    let sizes = farm_taught(&runs);
    assert_eq!(
        next(&sizes, "link.build"),
        (1000, GIB, SizedFrom::Class),
        "one run under a second: its own peak, its kind's CPU"
    );
    let tenth = Duration::from_secs(10);
    runs.extend(target_runs(
        &tables,
        ("link.build", "spread"),
        8,
        (16 * GIB, tenth),
        |run| if run == 7 { 50_000_000 } else { 10_000_000 },
    ));
    let sizes = farm_taught(&runs);
    let summary = sizes
        .summaries
        .get(&JobSizes::key_of(&queued("probe", step_spec("link.build"))))
        .unwrap();
    assert!(summary.mixed(), "50 MB is over four times the median 10 MB");
    assert_eq!(
        next(&sizes, "link.build").1,
        GIB,
        "nine runs, spread: its own peak"
    );
}

/// Asserts that one large target of a kind does not raise the other targets of the kind: beside
/// a compile that peaks at 9.3 GB (booked 10.75 GiB), a target whose run peaked at 2.8 GB books
/// 3.5 GiB, and each of twenty targets whose run peaked at 300 MB books 1 GiB, all `class`. So
/// the twenty-two book 34.25 GiB together where, each held at the kind's largest (10.75 GiB),
/// they booked 236.5 GiB. Catches a target with runs of its own held at its kind's largest
/// peak.
#[test]
fn one_large_target_in_a_kind_does_not_raise_the_others() {
    let tables = Tables::new();
    let mut runs = large_target(&tables);
    let wall = Duration::from_secs(60);
    runs.extend(target_runs(
        &tables,
        ("sql.build", "r"),
        1,
        (16 * GIB, wall),
        |_| 2_800_000_000,
    ));
    let small: Vec<String> = (0..20).map(|n| format!("small{n}.build")).collect();
    for class in &small {
        runs.extend(target_runs(
            &tables,
            (class, "r"),
            1,
            (16 * GIB, wall),
            |_| 300_000_000,
        ));
    }
    let sizes = farm_taught(&runs);
    let sql = next(&sizes, "sql.build");
    assert_eq!((sql.1, sql.2), (14 * 256 * MIB, SizedFrom::Class));
    let mut total = sql.1 + next(&sizes, "big.build").1;
    for class in &small {
        let (_, memory, from) = next(&sizes, class);
        assert_eq!((memory, from), (GIB, SizedFrom::Class), "{class}");
        total += memory;
    }
    assert_eq!(total, 137 * 256 * MIB, "34.25 GiB for 22 jobs");
}

/// Asserts that a class never seen, of a kind never seen, whose client has no run, books the
/// cold size (22 cores and 16 GiB, `cold`), and that under another client, whose runs are
/// their own history, the same class is still cold although this client's kind has runs.
/// Catches a class never seen booked from another client's runs, or below the cold size.
#[test]
fn a_class_never_seen_still_books_the_cold_size() {
    let tables = Tables::new();
    let sizes = farm_taught(&large_target(&tables));
    let stranger = queued_by(Some("stranger"), "new-next", step_spec("new.build"));
    assert_eq!(super::size(&sizes, &stranger), (22_000, 16 * GIB));
    assert_eq!(
        sizes.sized_from(&stranger, js_sizing::Day(0)),
        Some(SizedFrom::Cold)
    );
    let empty = farm_taught(&[]);
    assert_eq!(
        next(&empty, "new.build"),
        (22_000, 16 * GIB, SizedFrom::Cold)
    );
}

/// Asserts that a run that used more than its booking raises the next booking at once and is
/// taught as the success it was, with no floor: a class whose five runs peaked at 1 GB books
/// 1.25 GiB; one run of it that peaked at 6 GiB under that 1.25 GiB booking (the booking is no
/// limit, so the run went on and ended with exit 0) books the next job 7.5 GiB, and a run at
/// exactly its booking books its peak with the margin (1.75 GiB), not the booking. Catches a
/// run above its booking taken as one held at a limit (`min(peak, booking)`, which leaves the
/// class at a booking its jobs outgrow) and a success taught as a kill.
#[test]
fn a_job_that_used_more_than_its_booking_raises_the_next_booking_at_once() {
    let tables = Tables::new();
    let wall = Duration::from_secs(60);
    let mut runs = target_runs(&tables, ("grow.build", "r"), 5, (16 * GIB, wall), |_| {
        1_000_000_000
    });
    let sizes = farm_taught(&runs);
    assert_eq!(next(&sizes, "grow.build").1, 5 * 256 * MIB);
    runs.extend(target_runs(
        &tables,
        ("grow.build", "over"),
        1,
        (5 * 256 * MIB, wall),
        |_| 6 * GIB,
    ));
    let sizes = farm_taught(&runs);
    assert_eq!(
        next(&sizes, "grow.build"),
        (1000, 30 * 256 * MIB, SizedFrom::Class)
    );
    let key = JobSizes::key_of(&queued("probe", step_spec("grow.build")));
    assert_eq!(sizes.summaries.get(&key).unwrap().oom_floor(), 0, "no kill");

    let at = target_runs(
        &tables,
        ("full.build", "r"),
        1,
        (GIB + 256 * MIB, wall),
        |_| GIB + 256 * MIB,
    );
    assert_eq!(next(&farm_taught(&at), "full.build").1, 7 * 256 * MIB);
}
