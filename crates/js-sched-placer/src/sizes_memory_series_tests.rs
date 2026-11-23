//! A class books from the peak of its runs' non-file memory series when a run reports a whole
//! one with a point, and from the group's `memory.peak` (which counts the page cache of the
//! files the job read) otherwise. A class of 3 GiB peaks books 3.75 GiB (`1.2 x` plus 128 MiB,
//! rounded up to 256 MiB), one of 9 GiB peaks 11 GiB.

use std::time::Duration;

use js_jobs::JobEnd;
use js_types::MemorySeries;

use super::{GIB, JobSizes, MIB, booked, exited, needs, of_class, queued, ran_for, taught};

/// The wall time of a run: a minute.
const WALL: Duration = Duration::from_secs(60);

/// A series of `points` MiB at one second a point over `run_secs` seconds.
fn series(points: &[u32], partial: bool) -> MemorySeries {
    MemorySeries::new(
        1,
        points.to_vec(),
        u64::try_from(points.len()).unwrap() * 1_000_000,
        partial,
    )
    .unwrap()
}

/// An exit with code 0 that reports `peak` bytes as `memory.peak` and `series`; `keep_usage` false leaves no usage.
fn exit_with(peak: u64, series: Option<MemorySeries>, keep_usage: bool) -> JobEnd {
    let mut end = exited(0, peak, Duration::from_secs(30));
    if let JobEnd::Exited { output, .. } = &mut end {
        output.memory_series = series;
        if !keep_usage {
            output.usage = None;
        }
    }
    end
}

/// Sizes of the class `class` taught five runs of `wall` that each end as `end` gives, booked
/// 16 GiB, and the memory the next job of the class is booked.
fn booking(class: &str, wall: Duration, end: impl Fn() -> JobEnd) -> u64 {
    let tables = super::Tables::new();
    let jobs: Vec<_> = (0..5)
        .map(|at| {
            ran_for(
                &tables,
                (&format!("{class}-{at}"), None),
                of_class(class, needs(0, 0)),
                booked(4000, 16 * GIB),
                end(),
                wall,
            )
        })
        .collect();
    let sizes: JobSizes = taught(&jobs, booked(4000, 16 * GIB));
    super::size(&sizes, &queued("next", of_class(class, needs(0, 0)))).1
}

/// Asserts that runs whose `memory.peak` is 9 GiB (the page cache of what they read counted)
/// but whose series peaks at 3 GiB book 3.75 GiB, and the same runs with no series book 11 GiB
/// from `memory.peak`. Catches the page cache booked as memory the job needs, and a run with
/// no series taught nothing.
#[test]
fn a_series_peak_is_booked_not_the_memory_peak_and_a_run_without_one_books_the_memory_peak() {
    let with = booking("cached.build", WALL, || {
        exit_with(9 * GIB, Some(series(&[100, 3072, 200], false)), true)
    });
    assert_eq!(with, 15 * 256 * MIB);
    let without = booking("plain.build", WALL, || exit_with(9 * GIB, None, true));
    assert_eq!(without, 44 * 256 * MIB);
}

/// Asserts that a partial series (a run adopted after its agent restarted, whose series misses
/// its start) is ignored: runs with `memory.peak` 9 GiB and a partial series peaking at 3 GiB
/// book 11 GiB. Catches a series that misses the start of a run trusted as its peak.
#[test]
fn a_partial_series_is_ignored_for_the_memory_peak() {
    let memory = booking("partial.build", WALL, || {
        exit_with(9 * GIB, Some(series(&[3072], true)), true)
    });
    assert_eq!(memory, 44 * 256 * MIB);
}

/// Asserts that a whole series with no point falls back to `memory.peak`: 9 GiB books 11 GiB.
/// Catches an empty series taken as a peak of nothing (the class booked its 1 GiB least).
#[test]
fn an_empty_series_falls_back_to_the_memory_peak() {
    let memory = booking("empty.build", WALL, || {
        exit_with(9 * GIB, Some(series(&[], false)), true)
    });
    assert_eq!(memory, 44 * 256 * MIB);
}

/// Asserts that a run whose result has a series but no usage at all still teaches its series
/// peak (3 GiB books 3.75 GiB). Catches a series dropped with the usage it came beside.
#[test]
fn a_series_with_no_usage_still_teaches_its_peak() {
    let memory = booking("nousage.build", WALL, || {
        exit_with(0, Some(series(&[3072], false)), false)
    });
    assert_eq!(memory, 15 * 256 * MIB);
}

/// Asserts that a run under a second that reports a series books the series peak, not the
/// `memory.peak`: 400 ms runs of 3 GiB in the series and 9 GiB in `memory.peak` book 3.75 GiB.
/// Catches a short run's series ignored.
#[test]
fn a_run_under_a_second_with_a_series_uses_the_series_peak() {
    let memory = booking("brief.build", Duration::from_millis(400), || {
        exit_with(9 * GIB, Some(series(&[3072], false)), true)
    });
    assert_eq!(memory, 15 * 256 * MIB);
}

/// Asserts that the peak booked is the largest point of the series wherever it lies: first,
/// middle and last, each with a 3 GiB point among 1 GiB ones, book 3.75 GiB. Catches the first
/// or last point taken as the peak.
#[test]
fn the_peak_is_the_largest_point_wherever_it_is() {
    for (name, points) in [
        ("first.build", [3072, 1024, 512]),
        ("middle.build", [512, 3072, 1024]),
        ("last.build", [512, 1024, 3072]),
    ] {
        let memory = booking(name, WALL, || {
            exit_with(9 * GIB, Some(series(&points, false)), true)
        });
        assert_eq!(memory, 15 * 256 * MIB, "{name}");
    }
}
