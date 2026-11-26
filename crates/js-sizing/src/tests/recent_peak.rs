//! `Summaries::peak_memory`, what a key's own runs book: the nearest-rank 95th percentile of
//! its last 32 samples (the largest under 20 samples, the second largest from 20 on) with the
//! margin, at least its largest held sample of the last 256 or more runs and its kill floor.
//! One outlier run, and any run older than the last 32, no longer holds the key up.

use super::*;

fn peak(s: &Summaries, k: &Key) -> Option<u64> {
    s.peak_memory(k, DAY, &Bounds::default())
}

/// Rule, the farm's measured case: the `sql` `mojo_compile` target booked 15.0 GiB in the
/// window, from a run at 12.39 GiB (`1.2 x 12,690 MiB + 128 MiB` is 15 GiB) still in its
/// last 256, while its runs there peaked at 2.2 to 4.34 GiB. One such run among 30 books
/// nothing, and after the 70 runs it made in the window it books 5.5 GiB (`1.2 x 4,444 MiB +
/// 128 MiB`, rounded up to 256 MiB). Catches the largest peak of the last 256 runs (15 GiB,
/// as booked before).
#[test]
fn the_farms_sql_compile_books_its_recent_runs_not_an_old_outlier() {
    let mut s = Summaries::default();
    let k = linux(1);
    let runs = [2250, 2560, 3070, 3480, 3990, 4444].map(|mib| success(mib * MIB));
    for run in runs.iter().cycle().take(30) {
        feed(&mut s, &k, run, 1);
    }
    feed(&mut s, &k, &success(12_690 * MIB), 1);
    assert_eq!(
        peak(&s, &k),
        Some(11 * GIB / 2),
        "one outlier among 31 runs"
    );
    for run in runs.iter().cycle().take(70) {
        feed(&mut s, &k, run, 1);
    }
    assert_eq!(
        s.get(&k).unwrap().long_max(),
        12_690 * MIB,
        "still in the span"
    );
    assert_eq!(peak(&s, &k), Some(11 * GIB / 2));
}

/// Rule, boundary: under 20 samples the percentile is the largest, so a young key books its
/// heaviest run; from 20 on it is the second largest, so one outlier no longer books and two
/// do. Catches a percentile that drops an outlier of 19 runs, or keeps one of 20.
#[test]
fn one_outlier_among_20_runs_books_nothing_and_two_do() {
    let k = linux(1);
    for (light, books) in [(18, 39 * GIB / 4), (19, 3 * GIB / 2)] {
        let mut s = Summaries::default();
        feed(&mut s, &k, &success(GIB), light);
        feed(&mut s, &k, &success(8 * GIB), 1);
        assert_eq!(
            peak(&s, &k),
            Some(books),
            "{light} light runs and one outlier"
        );
        feed(&mut s, &k, &success(8 * GIB), 1);
        assert_eq!(peak(&s, &k), Some(39 * GIB / 4), "two outliers");
    }
}

/// Rule: only the last 32 samples count: once 32 light runs follow a heavy one, the heavy
/// run is out of the window, though still in the last 256. Catches the largest measured
/// sample of the last 256 runs (it booked 8.75 GiB for 256 runs).
#[test]
fn a_run_older_than_the_last_32_no_longer_books() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(7 * GIB), 1);
    feed(&mut s, &k, &success(GIB), 32);
    assert_eq!(s.get(&k).unwrap().long_max(), 7 * GIB);
    assert_eq!(peak(&s, &k), Some(3 * GIB / 2));
}

/// Rule, the safety kept: a run that reached its booking (a held sample) books a step above
/// that booking however alone it is among the last 32, and for the 256 runs after it; a kill
/// floor of the key's own books above every sample. Catches a held sample or a floor dropped
/// as an outlier.
#[test]
fn a_held_run_and_an_own_kill_floor_still_raise_the_key() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 31);
    let at_limit = Run {
        limit_hits: 1,
        booked_memory_bytes: 6 * GIB,
        ..success(6 * GIB)
    };
    feed(&mut s, &k, &at_limit, 1);
    // 6 GiB x 1.2 + 128 MiB = 7.325 GiB, rounded up to 7.5 GiB.
    assert_eq!(peak(&s, &k), Some(15 * GIB / 2), "the held booking");
    feed(&mut s, &k, &success(GIB), 200);
    assert_eq!(peak(&s, &k), Some(15 * GIB / 2), "held for 256 runs");
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    feed(&mut s, &k, &success(GIB), 40);
    assert_eq!(peak(&s, &k), Some(16 * GIB), "the kill's floor");
}
