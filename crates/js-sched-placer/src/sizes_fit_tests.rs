//! How the runs freed while serving did against what they booked ([`JobSizes::fit`]) and how
//! their checks against their classes' profiles came out ([`JobSizes::curve_checks`]): runs of
//! the job tables on the in-memory fake, every clock the test's.

use std::time::Duration;

use js_jobs::{Booked, JobEnd, JobRecord};
use js_types::{CpuSeries, ManualClock, MemorySeries, Timestamp};

use super::tests::{GIB, booked, exited, needs, of_class, output, ran, ran_for};
use super::{Curves, JobSizes};
use crate::engine::Settings;
use crate::engine::rig::Tables;
use crate::place::SchedPlacer;
use crate::telemetry::{CheckCounts, CurveChecks, RatioHistogram};

/// What every run of this module booked: two cores and 4 GiB.
fn bk() -> Booked {
    booked(2000, 4 * GIB)
}

/// A histogram holding one observation in each bucket of `buckets`, with `sum_permille`.
fn held(buckets: &[usize], sum_permille: u64) -> RatioHistogram {
    let mut counts = [0; 10];
    buckets.iter().for_each(|bucket| counts[*bucket] += 1);
    RatioHistogram {
        counts,
        sum_permille,
    }
}

/// A run of `class` that ended as `end` gives, booked as `booked`, ten seconds long.
fn ended(tables: &Tables, id: &str, booked: Booked, end: JobEnd) -> JobRecord {
    ran(tables, (id, None), of_class("c", needs(0, 0)), booked, end)
}

/// An exit with code 0 that used 2 GiB of memory and ten seconds of CPU time (a mean of one
/// core over its ten seconds), whose group's CPU peaked at 3000 millicores.
fn exit_with_peak(tweak: impl FnOnce(&mut js_jobs::ResourceUsage)) -> JobEnd {
    let JobEnd::Exited { mut output, .. } = exited(0, 2 * GIB, Duration::from_secs(10)) else {
        panic!("an exit");
    };
    let usage = output.usage.as_mut().unwrap();
    usage.cpu_peak.as_mut().unwrap().millicores = 3000;
    tweak(usage);
    JobEnd::Exited {
        exit_code: 0,
        output,
    }
}

/// Frees `job`, booked as `booked`, into `sizes`, as a pass serving does.
fn freed(sizes: &mut JobSizes, job: &JobRecord, booked: Booked) {
    sizes.freed_run(job.clone(), booked);
}

/// Behaviour: a success that booked two cores and 4 GiB, ran ten seconds on ten seconds of CPU
/// time (a mean of one core), peaked at three cores for five seconds and held 2 GiB, is one
/// sample of each ratio: its mean over its booking is 500 thousandths, its peak over its mean
/// 3000, its memory over its booking 500, each in the bucket at most its value; the sums are the
/// values. Catches a ratio read in the wrong unit and a sample in the wrong bucket.
#[test]
fn a_success_freed_while_serving_gives_its_cpu_mean_its_cpu_peak_and_its_memory_ratios() {
    let tables = Tables::new();
    let job = ended(&tables, "j", bk(), exit_with_peak(|_| {}));
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &job, bk());
    let fit = sizes.fit();
    assert_eq!(fit.cpu_mean_over_booked, held(&[1], 500));
    assert_eq!(fit.cpu_peak_over_mean, held(&[7], 3000));
    assert_eq!(fit.memory_peak_over_booked, held(&[1], 500));
    assert_eq!(fit.memory_kills, 0);
}

/// Behaviour: a run read from the history (what [`JobSizes::observe`] takes) is not counted,
/// and the same run freed while serving is. Catches a restart that fills the histograms with
/// the last days' runs.
#[test]
fn a_run_read_from_the_history_is_not_counted_in_the_fit() {
    let tables = Tables::new();
    let job = ended(&tables, "j", bk(), exit_with_peak(|_| {}));
    let mut sizes = JobSizes::new();
    let _ = sizes.observe(&job, bk());
    assert_eq!(sizes.fit().cpu_mean_over_booked.count(), 0);
    assert_eq!(sizes.fit().cpu_peak_over_mean.count(), 0);
    assert_eq!(sizes.fit().memory_peak_over_booked.count(), 0);
    freed(&mut sizes, &job, bk());
    assert_eq!(sizes.fit().memory_peak_over_booked.count(), 1);
}

/// Behaviour: a success that used no CPU time gives no CPU ratio (neither its mean nor its
/// peak over a mean of nothing) and still gives its memory's; a success under a second has no
/// CPU sample either, as the estimator takes none, but gives its memory's. Catches a zero
/// counted as a run that used a quarter of its booking or less.
#[test]
fn a_success_with_no_cpu_sample_gives_its_memory_ratio_alone() {
    let tables = Tables::new();
    let idle = exited(0, 2 * GIB, Duration::ZERO);
    let idle = ended(&tables, "idle", bk(), idle);
    let brief = exited(0, 2 * GIB, Duration::from_secs(1));
    let brief = ran_for(
        &tables,
        ("brief", None),
        of_class("c", needs(0, 0)),
        bk(),
        brief,
        Duration::from_millis(500),
    );
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &idle, bk());
    freed(&mut sizes, &brief, bk());
    let fit = sizes.fit();
    assert_eq!(fit.cpu_mean_over_booked.count(), 0);
    assert_eq!(fit.cpu_peak_over_mean.count(), 0);
    assert_eq!(fit.memory_peak_over_booked, held(&[1, 1], 1000));
}

/// Behaviour: a success whose host reported no CPU peak gives its mean over its booking and its
/// memory ratio and no peak over mean. Catches a missing peak counted as a peak of nothing.
#[test]
fn a_success_with_no_cpu_peak_gives_no_peak_over_mean() {
    let tables = Tables::new();
    let end = exit_with_peak(|usage| usage.cpu_peak = None);
    let job = ended(&tables, "j", bk(), end);
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &job, bk());
    let fit = sizes.fit();
    assert_eq!(fit.cpu_mean_over_booked.count(), 1);
    assert_eq!(fit.cpu_peak_over_mean.count(), 0);
    assert_eq!(fit.memory_peak_over_booked.count(), 1);
}

/// Behaviour: a success whose host reported no usage at all (no CPU time, no peak, no memory)
/// gives no sample of any ratio. Catches a run with nothing measured counted as one that used
/// nothing.
#[test]
fn a_success_with_no_usage_gives_no_ratio() {
    let tables = Tables::new();
    let JobEnd::Exited { mut output, .. } = exited(0, GIB, Duration::from_secs(10)) else {
        panic!("an exit");
    };
    output.usage = None;
    let end = JobEnd::Exited {
        exit_code: 0,
        output,
    };
    let job = ended(&tables, "j", bk(), end);
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &job, bk());
    let fit = sizes.fit();
    assert_eq!(fit.cpu_mean_over_booked.count(), 0);
    assert_eq!(fit.cpu_peak_over_mean.count(), 0);
    assert_eq!(fit.memory_peak_over_booked.count(), 0);
}

/// Behaviour: a success that booked no CPU and no memory gives neither ratio over a booking
/// (there is none to be over) and only its peak over its mean. Catches a division by a booking
/// of nothing.
#[test]
fn a_success_that_booked_nothing_gives_no_ratio_over_its_booking() {
    let tables = Tables::new();
    let nothing = booked(0, 0);
    let job = ended(&tables, "j", nothing, exit_with_peak(|_| {}));
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &job, nothing);
    let fit = sizes.fit();
    assert_eq!(fit.cpu_mean_over_booked.count(), 0);
    assert_eq!(fit.cpu_peak_over_mean, held(&[7], 3000));
    assert_eq!(fit.memory_peak_over_booked.count(), 0);
}

/// Behaviour: a ratio too large for a `u64` is held at `u64::MAX`, in the last bucket, and the
/// sum stops there. Catches a wrapped ratio counted as a small one.
#[test]
fn a_ratio_past_u64_is_held_at_the_largest_and_in_the_last_bucket() {
    let tables = Tables::new();
    let job = ended(&tables, "j", booked(2000, 1), exit_with_peak(|_| {}));
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &job, booked(2000, 1));
    let JobEnd::Exited { mut output, .. } = exited(0, 1 << 60, Duration::from_secs(10)) else {
        panic!("an exit");
    };
    output.usage.as_mut().unwrap().peak_memory_bytes = 1 << 60;
    let huge = ended(
        &tables,
        "huge",
        booked(2000, 1),
        JobEnd::Exited {
            exit_code: 0,
            output,
        },
    );
    freed(&mut sizes, &huge, booked(2000, 1));
    let fit = &sizes.fit().memory_peak_over_booked;
    assert_eq!(fit.counts[9], 2);
    assert_eq!(fit.sum_permille, u64::MAX);
}

/// Behaviour: a run stopped for memory is counted as a kill and gives no ratio; a run that
/// failed (an exit with a code) is neither a kill nor a sample. Catches a kill counted as a
/// sample of the memory it died at, and a failure counted as a kill.
#[test]
fn a_memory_kill_is_counted_and_a_failure_is_not() {
    let tables = Tables::new();
    let killed = JobEnd::Memory {
        exit_code: None,
        output: output(4 * GIB, Duration::from_secs(5)),
    };
    let killed = ended(&tables, "killed", bk(), killed);
    let failed = exited(1, 2 * GIB, Duration::from_secs(10));
    let failed = ended(&tables, "failed", bk(), failed);
    let mut sizes = JobSizes::new();
    freed(&mut sizes, &failed, bk());
    assert_eq!(sizes.fit().memory_kills, 0);
    freed(&mut sizes, &killed, bk());
    let fit = sizes.fit();
    assert_eq!(fit.memory_kills, 1);
    assert_eq!(fit.cpu_mean_over_booked.count(), 0);
    assert_eq!(fit.cpu_peak_over_mean.count(), 0);
    assert_eq!(fit.memory_peak_over_booked.count(), 0);
}

/// A CPU series of `points` sixteenths of a core, one second a point.
fn cpu_series(points: &[u16], partial: bool) -> CpuSeries {
    let run_usec = u64::try_from(points.len()).unwrap() * 1_000_000;
    CpuSeries::new(1, points.to_vec(), run_usec, partial).unwrap()
}

/// A memory series of `points` MiB, one second a point.
fn memory_series(points: &[u32], partial: bool) -> MemorySeries {
    let run_usec = u64::try_from(points.len()).unwrap() * 1_000_000;
    MemorySeries::new(1, points.to_vec(), run_usec, partial).unwrap()
}

/// A run of the class `class` that exited with `code`, recording `cpu` and `memory`; its CPU
/// time is what its CPU series holds (ten seconds without one) and `tweak` changes what the host
/// reported.
fn series_run(
    tables: &Tables,
    (id, class): (&str, &str),
    code: i32,
    (cpu, memory): (Option<CpuSeries>, Option<MemorySeries>),
    tweak: impl FnOnce(&mut js_jobs::Output),
) -> JobRecord {
    // A sixteenth of a core for a second is 62 500 microseconds.
    let cpu_time = cpu.as_ref().map_or(Duration::from_secs(10), |series| {
        let sixteenths: u64 = series.points.iter().map(|point| u64::from(*point)).sum();
        Duration::from_micros(sixteenths * 62_500)
    });
    let JobEnd::Exited { mut output, .. } = exited(code, GIB, cpu_time) else {
        panic!("an exit");
    };
    output.cpu_series = cpu;
    output.memory_series = memory;
    tweak(&mut output);
    let end = JobEnd::Exited {
        exit_code: code,
        output,
    };
    ran(tables, (id, None), of_class(class, needs(0, 0)), bk(), end)
}

/// Behaviour: each run freed while serving is counted under the status of its check, for the
/// CPU and for the memory: one learned, one that only raises its class and so is not learned
/// (CPU alone: its host saw it wait half its wall time for a CPU), one with no series, one with
/// a series that misses its start, one whose CPU series disagrees with its CPU time (its memory
/// is learned beside it) and one that failed. Catches a status counted under another, and a
/// dimension that takes the other's status.
#[test]
fn each_run_freed_is_counted_by_the_status_of_its_checks() {
    let tables = Tables::new();
    let whole = || {
        (
            Some(cpu_series(&[16; 10], false)),
            Some(memory_series(&[1024; 10], false)),
        )
    };
    let mut sizes = JobSizes::new().with_curves(Curves::Shadow);
    let runs = [
        series_run(&tables, ("a0", "a"), 0, whole(), |_| {}),
        series_run(&tables, ("a1", "a"), 0, whole(), |output| {
            output.usage.as_mut().unwrap().cpu_pressure_some = Some(Duration::from_secs(5));
        }),
        series_run(&tables, ("a2", "a"), 0, (None, None), |_| {}),
        series_run(
            &tables,
            ("a3", "a"),
            0,
            (
                Some(cpu_series(&[16; 10], true)),
                Some(memory_series(&[1024; 10], true)),
            ),
            |_| {},
        ),
        series_run(&tables, ("a4", "a"), 0, whole(), |output| {
            output.usage.as_mut().unwrap().cpu_time = Duration::from_secs(20);
        }),
        series_run(&tables, ("a5", "a"), 1, whole(), |_| {}),
    ];
    for job in &runs {
        freed(&mut sizes, job, bk());
    }
    let expected = CurveChecks {
        cpu: CheckCounts {
            learned: 1,
            not_learned: 1,
            no_series: 1,
            partial: 1,
            disagrees: 1,
            not_a_success: 1,
            overran: 0,
        },
        memory: CheckCounts {
            learned: 3,
            not_learned: 0,
            no_series: 1,
            partial: 1,
            disagrees: 0,
            not_a_success: 1,
            overran: 0,
        },
    };
    assert_eq!(sizes.curve_checks(), expected);
}

/// Behaviour: a run whose CPU peak is more than a quarter above what its class's profile
/// predicted of the run is counted as learned and as an overrun; the eight runs before it,
/// which the profile had nothing to predict from, are not. The memory, which it recorded no
/// series of, has no overrun. Catches an overrun counted for a run that was not scored.
#[test]
fn a_run_past_its_predicted_peak_is_counted_as_an_overrun() {
    let tables = Tables::new();
    let phased = |burst: u16| {
        let mut points = vec![0; 30];
        points[20..24].fill(burst);
        points
    };
    let series = |burst: u16| (Some(cpu_series(&phased(burst), false)), None);
    let mut sizes = JobSizes::new().with_curves(Curves::Shadow);
    for at in 0..8 {
        let job = series_run(&tables, (&format!("p{at}"), "p"), 0, series(32), |_| {});
        freed(&mut sizes, &job, bk());
    }
    assert_eq!(sizes.curve_checks().cpu.overran, 0);
    let ninth = series_run(&tables, ("p8", "p"), 0, series(48), |_| {});
    freed(&mut sizes, &ninth, bk());
    let checks = sizes.curve_checks();
    assert_eq!((checks.cpu.learned, checks.cpu.overran), (9, 1));
    assert_eq!(checks.memory.no_series, 9);
    assert_eq!(checks.memory.overran, 0);
}

/// Behaviour: the checks are counted for the runs freed while serving, not for the runs read
/// from the history, and not at all with the profiles off. Catches a restart that counts the
/// last days' runs, and a count kept where no profile is.
#[test]
fn the_checks_are_counted_only_for_runs_freed_with_profiles_kept() {
    let tables = Tables::new();
    let series = || {
        (
            Some(cpu_series(&[16; 10], false)),
            Some(memory_series(&[1024; 10], false)),
        )
    };
    let job = series_run(&tables, ("j", "j"), 0, series(), |_| {});
    let mut shadow = JobSizes::new().with_curves(Curves::Shadow);
    let _ = shadow.observe(&job, bk());
    assert_eq!(shadow.curve_checks(), CurveChecks::default());
    let mut off = JobSizes::new();
    freed(&mut off, &job, bk());
    assert_eq!(off.curve_checks(), CurveChecks::default());
    freed(&mut shadow, &job, bk());
    assert_eq!(shadow.curve_checks().cpu.learned, 1);
}

/// Behaviour: the placer's telemetry carries the fit and the checks of the runs its history
/// took freed while serving. Catches a telemetry that leaves them out.
#[test]
fn the_placer_telemetry_carries_the_fit_and_the_checks_of_the_runs_freed() {
    let tables = Tables::new();
    let series = (
        Some(cpu_series(&[16; 10], false)),
        Some(memory_series(&[1024; 10], false)),
    );
    let job = series_run(&tables, ("j", "j"), 0, series, |_| {});
    let settings = Settings {
        curves: Curves::Shadow,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, ManualClock::new(Timestamp::from_millis(0)));
    assert_eq!(placer.telemetry().fit.memory_kills, 0);
    placer.engine().sizes().freed_run(job, bk());
    let telemetry = placer.telemetry();
    assert_eq!(telemetry.fit.cpu_mean_over_booked, held(&[1], 500));
    assert_eq!(telemetry.checks.cpu.learned, 1);
    assert_eq!(telemetry.checks.memory.learned, 1);
}
