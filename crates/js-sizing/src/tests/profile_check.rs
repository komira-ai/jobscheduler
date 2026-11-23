//! A profile checks each run against itself before it learns it: what is scored, when a
//! run is learned, how many runs and scores are kept, and when a class is shaped.

use super::*;
use crate::profile::shaped_by;

/// When the runs of these tests end.
const E: u64 = 10 * HALF_LIFE_MS;

/// A series of one-second points whose run lasts exactly its points.
fn pts(points: &[u32]) -> RunSeries<'_> {
    RunSeries {
        granularity_secs: 1,
        points,
        run_usec: points.len() as u64 * 1_000_000,
    }
}

/// Feeds `points` as `n` clean runs ending at `E`.
fn learn(p: &mut Profile, points: &[u32], n: usize) {
    for _ in 0..n {
        p.check_and_learn(pts(points), E, true);
    }
}

/// A profile of the CPU unit that learned `n` clean runs of `points`.
fn class(points: &[u32], n: usize) -> Profile {
    let mut p = Profile::new(CPU_UNIT);
    learn(&mut p, points, n);
    p
}

/// A first clean run on an empty profile is learned and not scored: it reports its own
/// mean and peak and nothing predicted.
#[test]
fn the_first_run_is_learned_and_not_scored() {
    let mut p = Profile::new(CPU_UNIT);
    let c = p.check_and_learn(pts(&[4, 8, 6, 2]), E, true);
    assert_eq!(
        c,
        Checked {
            learned: true,
            runs: 1,
            actual_mean: 5,
            actual_peak: 6,
            predicted_level: None,
            predicted_peak: None,
            curve_error: None,
            flat_error: None,
            window: Window::default(),
            shaped: false,
        }
    );
}

/// A series with no point teaches and scores nothing, clean or not, and leaves the
/// profile and its window as they were.
#[test]
fn an_empty_series_changes_nothing() {
    let mut p = class(&[10, 10, 10, 10], 9);
    let before = p.clone();
    let none = RunSeries {
        granularity_secs: 1,
        points: &[],
        run_usec: 7_000_000,
    };
    for clean in [true, false] {
        let c = p.check_and_learn(none, E, clean);
        assert!(!c.learned);
        assert_eq!(c.runs, 9);
        assert_eq!((c.actual_mean, c.actual_peak), (0, 0));
        assert_eq!(
            (
                c.predicted_level,
                c.predicted_peak,
                c.curve_error,
                c.flat_error
            ),
            (None, None, None, None)
        );
        assert_eq!(c.window, p.window());
        assert!(!c.shaped);
    }
    assert_eq!(p, before);
}

/// A run is scored only when the profile already kept 8 runs: the 8th run (7 kept) is
/// not, the 9th (8 kept) is, and a run like the others costs nothing in either measure.
#[test]
fn a_run_is_scored_from_eight_kept_runs() {
    let mut p = Profile::new(CPU_UNIT);
    for _ in 0..MIN_RUNS {
        let c = p.check_and_learn(pts(&[10; 4]), E, true);
        assert_eq!((c.curve_error, c.flat_error), (None, None));
        assert_eq!(c.predicted_peak, None);
        assert_eq!(c.window.runs, 0);
    }
    let c = p.check_and_learn(pts(&[10; 4]), E, true);
    assert_eq!((c.curve_error, c.flat_error), (Some(0), Some(0)));
    assert_eq!(c.predicted_level, Some(10));
    assert_eq!(c.predicted_peak, Some(10));
    assert_eq!(c.window.runs, 1);
    assert!(!c.shaped);
}

/// A run twice as busy as its class is off by 10 in each of its two slots for 2 s each,
/// against the curve and the flat level alike, and is learned.
#[test]
fn a_busier_run_is_scored_by_second() {
    let mut p = class(&[10; 4], 8);
    let c = p.check_and_learn(pts(&[20; 4]), E, true);
    assert!(c.learned);
    assert_eq!(c.runs, 9);
    assert_eq!((c.actual_mean, c.actual_peak), (20, 20));
    assert_eq!(c.predicted_level, Some(10));
    assert_eq!(c.predicted_peak, Some(10));
    assert_eq!((c.curve_error, c.flat_error), (Some(40), Some(40)));
    assert_eq!(
        c.window,
        Window {
            runs: 1,
            curve_error: 40,
            flat_error: 40
        }
    );
    assert_eq!(c.window.fit_permille(), Some(1000));
    assert_eq!(p.level(E), Some(11));
}

/// A slot reached by three kept runs predicts the level; reached by four it predicts its
/// own mean. The same run is scored against each.
#[test]
fn a_slot_predicts_its_mean_from_four_runs() {
    let long = [10, 10, 10, 10, 50, 50];
    let mut three = class(&[10; 4], 5);
    learn(&mut three, &long, 3);
    assert_eq!(three.slot_runs(2), 3);
    assert_eq!(three.level(E), Some(16));
    let c = three.check_and_learn(pts(&long), E, true);
    assert_eq!((c.curve_error, c.flat_error), (Some(68), Some(92)));
    assert_eq!(c.predicted_peak, Some(50));

    let mut four = class(&[10; 4], 4);
    learn(&mut four, &long, 4);
    assert_eq!(four.slot_runs(2), 4);
    assert_eq!(four.level(E), Some(18));
    let c = four.check_and_learn(pts(&long), E, true);
    assert_eq!((c.curve_error, c.flat_error), (Some(0), Some(96)));
    assert_eq!(c.predicted_peak, Some(50));
}

/// Seconds past 128 are scored by their point against the tail's mean (curve) and the
/// level (flat): 70 and 90 against a tail of 40 and a level of 10.
#[test]
fn tail_seconds_are_scored_against_the_tail() {
    let mut base = vec![10; 128];
    base.extend([30, 50]);
    let mut p = class(&base, 8);
    assert_eq!(p.tail(E), Some(40));
    assert_eq!(p.level(E), Some(10));
    let mut run = vec![10; 128];
    run.extend([70, 90]);
    let c = p.check_and_learn(pts(&run), E, true);
    assert_eq!((c.curve_error, c.flat_error), (Some(80), Some(140)));
    assert_eq!((c.actual_mean, c.actual_peak), (11, 90));
    assert_eq!(c.predicted_peak, Some(10));
}

/// A run with no tail is not scored on a tail, and its slots beyond what the class
/// reached are scored against the level.
#[test]
fn a_run_longer_than_the_class_scores_its_new_slots_against_the_level() {
    let mut p = class(&[10; 4], 8);
    let c = p.check_and_learn(pts(&[10, 10, 10, 10, 30, 30]), E, true);
    assert_eq!((c.curve_error, c.flat_error), (Some(40), Some(40)));
    assert_eq!(c.predicted_peak, Some(10));
}

/// When every kept run is too old to weigh, the level is unknown and the run is scored
/// against 0, with a predicted peak of 0.
#[test]
fn a_run_checked_against_weightless_runs_is_scored_against_zero() {
    let mut p = class(&[10, 10], 8);
    let c = p.check_and_learn(pts(&[10, 10]), E + 100 * HALF_LIFE_MS, true);
    assert!(c.learned);
    assert_eq!(c.predicted_level, None);
    assert_eq!(c.predicted_peak, Some(0));
    assert_eq!((c.curve_error, c.flat_error), (Some(20), Some(20)));
    assert_eq!(c.actual_mean, 10);
}

/// The profile keeps its last 64 runs: the 65th pushes out the first, so the level is
/// that of the last 64.
#[test]
fn the_profile_keeps_the_last_64_runs() {
    let mut p = class(&[10, 10], MAX_RUNS);
    assert_eq!(p.runs(), MAX_RUNS);
    let c = p.check_and_learn(pts(&[1010, 1010]), E, true);
    assert_eq!(c.runs, MAX_RUNS);
    assert_eq!(p.runs(), MAX_RUNS);
    assert_eq!(p.level(E), Some(26));
    let mut q = Profile::new(CPU_UNIT);
    q.check_and_learn(pts(&[1000, 1000]), E, true);
    learn(&mut q, &[10, 10], MAX_RUNS);
    assert_eq!(q.runs(), MAX_RUNS);
    assert_eq!(q.level(E), Some(10));
}

/// The window holds the last 16 scores: after every call its count and sums equal those
/// of the last 16 scores the calls reported.
#[test]
fn the_window_keeps_the_last_16_scores() {
    let mut p = Profile::new(CPU_UNIT);
    let mut scores: Vec<(u64, u64)> = Vec::new();
    for i in 0..40u32 {
        let v = 10 + (i % 7) * 5;
        let c = p.check_and_learn(pts(&[v; 4]), E, true);
        if let (Some(curve), Some(flat)) = (c.curve_error, c.flat_error) {
            scores.push((curve, flat));
        }
        let last = &scores[scores.len().saturating_sub(CHECK_RUNS)..];
        assert_eq!(c.window.runs, last.len());
        assert_eq!(c.window.curve_error, last.iter().map(|s| s.0).sum::<u64>());
        assert_eq!(c.window.flat_error, last.iter().map(|s| s.1).sum::<u64>());
        assert_eq!(p.window(), c.window);
    }
    assert!(scores.len() > CHECK_RUNS);
    assert!(scores.iter().any(|s| s.0 > 0));
}

/// A class whose runs all burst at 20 to 24 s is shaped once 8 runs are kept and 16 are
/// scored (the 24th run), not before: the curve knows the burst and flat does not.
#[test]
fn a_phased_class_becomes_shaped_after_24_runs() {
    let run: Vec<u32> = (0..40)
        .map(|s| if (20..24).contains(&s) { 100 } else { 10 })
        .collect();
    let mut p = Profile::new(CPU_UNIT);
    for i in 1..=30 {
        let c = p.check_and_learn(pts(&run), E, true);
        assert_eq!(c.shaped, i >= 24, "run {i}");
        assert_eq!(p.shaped(), c.shaped);
    }
    let w = p.window();
    assert_eq!(w.runs, CHECK_RUNS);
    assert_eq!(w.curve_error, 0);
    assert!(w.flat_error > 0);
    assert_eq!(w.fit_permille(), Some(0));
}

/// A class whose runs differ only in level has no shape: the curve is the level, its
/// error equals flat's (1000 per mille, above 800) and the class stays flat.
#[test]
fn a_class_of_one_slot_stays_flat() {
    let mut p = Profile::new(CPU_UNIT);
    for i in 0..30 {
        let v = if i % 2 == 0 { 10 } else { 30 };
        p.check_and_learn(pts(&[v, v]), E, true);
    }
    let w = p.window();
    assert_eq!(w.runs, CHECK_RUNS);
    assert!(w.flat_error > 0);
    assert_eq!(w.fit_permille(), Some(1000));
    assert!(!p.shaped());
}

/// A class that never varies has no error to explain and stays flat.
#[test]
fn a_constant_class_stays_flat() {
    let p = class(&[10; 4], 30);
    let w = p.window();
    assert_eq!(w.runs, CHECK_RUNS);
    assert_eq!((w.curve_error, w.flat_error), (0, 0));
    assert_eq!(w.fit_permille(), None);
    assert!(!p.shaped());
}

/// Shaped needs 8 kept runs, a full window, some flat error and a curve error of at
/// most 800 per mille: exactly 800 is shaped, 801 is not.
#[test]
fn shaped_needs_each_of_its_four_conditions() {
    let full = Window {
        runs: CHECK_RUNS,
        curve_error: 800,
        flat_error: 1000,
    };
    assert!(shaped_by(MIN_RUNS, full));
    assert!(!shaped_by(MIN_RUNS - 1, full));
    assert!(!shaped_by(
        MIN_RUNS,
        Window {
            curve_error: 801,
            ..full
        }
    ));
    assert!(!shaped_by(
        MIN_RUNS,
        Window {
            runs: CHECK_RUNS - 1,
            ..full
        }
    ));
    assert!(!shaped_by(
        MIN_RUNS,
        Window {
            curve_error: 0,
            flat_error: 0,
            ..full
        }
    ));
    assert!(shaped_by(
        MIN_RUNS,
        Window {
            curve_error: 0,
            flat_error: 1,
            ..full
        }
    ));
}

/// The fit is the curve's error per mille of flat's; it is unknown when flat's error is
/// 0, and it does not overflow.
#[test]
fn the_fit_is_per_mille_of_flat() {
    let w = Window {
        runs: CHECK_RUNS,
        curve_error: 800,
        flat_error: 1000,
    };
    assert_eq!(w.fit_permille(), Some(800));
    assert_eq!(Window { flat_error: 0, ..w }.fit_permille(), None);
    assert_eq!(
        Window {
            curve_error: u64::MAX,
            flat_error: 1,
            ..w
        }
        .fit_permille(),
        Some(u64::MAX)
    );
}

/// A run that is not clean is learned only when its mean is above the level: below and
/// equal are not, above is.
#[test]
fn an_unclean_run_may_only_raise_the_class() {
    let mut p = class(&[10, 10], 1);
    let below = p.check_and_learn(pts(&[5, 5]), E, false);
    assert!(!below.learned);
    assert_eq!((below.runs, below.actual_mean), (1, 5));
    assert_eq!(below.predicted_level, Some(10));
    let equal = p.check_and_learn(pts(&[10, 10]), E, false);
    assert!(!equal.learned);
    assert_eq!(p.runs(), 1);
    let above = p.check_and_learn(pts(&[20, 20]), E, false);
    assert!(above.learned);
    assert_eq!(above.runs, 2);
    assert_eq!(p.level(E), Some(15));
}

/// An empty profile learns only a clean run: an unclean one is reported but not kept.
#[test]
fn an_unclean_run_is_not_learned_by_an_empty_profile() {
    let mut p = Profile::new(CPU_UNIT);
    let c = p.check_and_learn(pts(&[20, 20]), E, false);
    assert!(!c.learned);
    assert_eq!(c.runs, 0);
    assert_eq!(c.actual_mean, 20);
    assert_eq!(c.predicted_level, None);
    assert_eq!(p.runs(), 0);
}

/// An unclean run is scored like a clean one, even when it is not learned.
#[test]
fn an_unlearned_run_is_still_scored() {
    let mut p = class(&[10; 4], 8);
    let c = p.check_and_learn(pts(&[5; 4]), E, false);
    assert!(!c.learned);
    assert_eq!((c.curve_error, c.flat_error), (Some(20), Some(20)));
    assert_eq!(c.window.runs, 1);
    assert_eq!(p.runs(), 8);
}
