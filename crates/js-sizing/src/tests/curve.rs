//! The booked curve of a class: the slot an age falls in, the four checks, what each slot
//! books and what a running job is still charged ahead of it.

use super::*;

/// When the runs of these tests end.
const E: u64 = 10 * HALF_LIFE_MS;

/// The clamp of most tests: wide, so only the test of the clamp sees it.
const WIDE: Clamp = Clamp {
    least_millis: 0,
    most_millis: 100_000,
};

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

/// A window of 16 runs of which `usable` were usable.
fn series(usable: usize) -> SeriesWindow {
    let mut w = SeriesWindow::default();
    for i in 0..CHECK_RUNS {
        w.push(i < usable);
    }
    w
}

/// A class of 8 runs that each used 0 for 2 s then 180 for 2 s, then `fits` scored runs
/// whose error is 800 per mille of flat's and `spikes` scored runs that overran its
/// predicted peak of 180. The scored runs are not learned: their mean is not above the
/// level.
fn scored(fits: usize, spikes: usize) -> Profile {
    let mut p = class(&[0, 0, 180, 180], 8);
    for _ in 0..fits {
        p.check_and_learn(pts(&[40, 40, 140, 140]), E, false);
    }
    let spike = [0, 0, 226, 226, 0, 0, 0, 0, 0, 0, 0, 0];
    for _ in 0..spikes {
        p.check_and_learn(pts(&spike), E, false);
    }
    p
}

/// A job's slot is its age over 2 s, until 128 s: 1999 ms is still slot 0, 2000 ms slot 1,
/// 127 999 ms slot 63, and 128 000 ms and later the tail slot.
#[test]
fn an_age_falls_in_its_two_second_slot() {
    assert_eq!(age_slot(0), 0);
    assert_eq!(age_slot(1999), 0);
    assert_eq!(age_slot(2000), 1);
    assert_eq!(age_slot(127_999), 63);
    assert_eq!(age_slot(128_000), TAIL_SLOT);
    assert_eq!(age_slot(u64::MAX), TAIL_SLOT);
    assert_eq!((TAIL_SLOT, CURVE_SLOTS), (64, 65));
}

/// The series window keeps the last 16 runs: the 17th push drops the first, and `usable`
/// counts the usable ones left.
#[test]
fn the_series_window_keeps_the_last_16_runs() {
    let mut w = SeriesWindow::default();
    assert_eq!((w.runs(), w.usable()), (0, 0));
    w.push(true);
    for _ in 1..CHECK_RUNS {
        w.push(false);
    }
    assert_eq!((w.runs(), w.usable()), (CHECK_RUNS, 1));
    w.push(false);
    assert_eq!((w.runs(), w.usable()), (CHECK_RUNS, 0));
    w.push(true);
    assert_eq!((w.runs(), w.usable()), (CHECK_RUNS, 1));
}

/// A class that scored 16 runs, fits at 800 per mille, overran 3 times and has 12 usable
/// series passes all four checks.
#[test]
fn a_class_that_meets_every_bound_passes_the_checks() {
    let p = scored(13, 3);
    assert_eq!(p.overruns(), 3);
    let c = Checks::of(&p, &series(12));
    assert_eq!(
        c,
        Checks {
            scored: true,
            shaped: true,
            calibrated: true,
            usable: true,
        }
    );
    assert!(c.pass());
}

/// Check 1: 16 scored runs pass, 15 do not.
#[test]
fn check_one_needs_sixteen_scored_runs() {
    assert!(Checks::of(&scored(16, 0), &series(16)).scored);
    let c = Checks::of(&scored(15, 0), &series(16));
    assert!(!c.scored);
    assert!(!c.pass());
}

/// Check 2 is the profile's shape rule: a window whose error is exactly 800 per mille of
/// flat's passes, one whose first run is a little better passes, and one run a little
/// worse (802 per mille) fails.
#[test]
fn check_two_is_the_fit_at_800_per_mille() {
    let fit = |d: u32| {
        let mut c = class(&[0, 0, 180, 180], 8);
        for i in 0..CHECK_RUNS {
            let d = if i == 0 { d } else { 40 };
            c.check_and_learn(pts(&[d, d, 180 - d, 180 - d]), E, false);
        }
        c
    };
    assert_eq!(fit(40).window().fit_permille(), Some(800));
    assert!(Checks::of(&fit(40), &series(16)).shaped);
    assert!(Checks::of(&fit(39), &series(16)).shaped);
    let over = fit(41);
    assert_eq!(over.window().fit_permille(), Some(802));
    let c = Checks::of(&over, &series(16));
    assert!(!c.shaped);
    assert!(c.scored && c.calibrated && c.usable);
    assert!(!c.pass());
}

/// Check 3: 3 overruns among the 16 scored runs pass, 4 do not.
#[test]
fn check_three_allows_three_overruns() {
    assert!(Checks::of(&scored(13, 3), &series(16)).calibrated);
    let c = Checks::of(&scored(12, 4), &series(16));
    assert!(!c.calibrated);
    assert!(!c.pass());
}

/// Check 4: 12 usable series among the last 16 runs pass, 11 do not.
#[test]
fn check_four_needs_twelve_usable_series() {
    let p = scored(16, 0);
    assert!(Checks::of(&p, &series(12)).usable);
    let c = Checks::of(&p, &series(11));
    assert!(!c.usable);
    assert!(!c.pass());
    assert!(!Checks::of(&p, &SeriesWindow::default()).usable);
}

/// A class passes only when every check does: each one false alone fails it.
#[test]
fn a_class_passes_only_when_all_four_checks_hold() {
    let all = Checks {
        scored: true,
        shaped: true,
        calibrated: true,
        usable: true,
    };
    assert!(all.pass());
    assert!(
        !Checks {
            scored: false,
            ..all
        }
        .pass()
    );
    assert!(
        !Checks {
            shaped: false,
            ..all
        }
        .pass()
    );
    assert!(
        !Checks {
            calibrated: false,
            ..all
        }
        .pass()
    );
    assert!(
        !Checks {
            usable: false,
            ..all
        }
        .pass()
    );
}

/// A class of runs of 2 cores: `long` runs of 6 s and the rest of 8 of 4 s. Slots 0 and
/// 1 are reached by 8 runs, slot 2 by `long`.
fn two_cores(long: usize) -> Profile {
    let mut p = class(&[32; 4], 8 - long);
    learn(&mut p, &[32; 6], long);
    p
}

/// A slot reached by 3 kept runs books the flat number, one reached by 4 books its 90th
/// percentile (32 sixteenths, 2000 m); a slot no run reached books flat.
#[test]
fn a_slot_books_its_percentile_from_four_runs() {
    let three = Curve::of(&two_cores(3), 1500, WIDE, E);
    let four = Curve::of(&two_cores(4), 1500, WIDE, E);
    assert_eq!((three.booking(0), three.booking(1)), (2000, 2000));
    assert_eq!((three.booking(2), four.booking(2)), (1500, 2000));
    assert_eq!((three.reach(2), four.reach(2)), (3, 4));
    assert_eq!((three.reach(0), three.reach(3)), (8, 0));
    assert_eq!(four.booking(3), 1500);
    assert_eq!(four.booking(TAIL_SLOT), 1500);
    assert_eq!(four.reach(TAIL_SLOT), 0);
}

/// The tail books its mean past 128 s with 4 runs that lasted that long, flat with 3; a
/// run of exactly 128 s is not one of them.
#[test]
fn the_tail_books_its_mean_from_four_runs() {
    let tail = |long: usize, exact: usize| {
        let mut p = class(&[32; 128], exact);
        learn(&mut p, &[32; 130], long);
        Curve::of(&p, 1500, WIDE, E)
    };
    let (three, four) = (tail(3, 5), tail(4, 4));
    assert_eq!(three.booking(TAIL_SLOT), 1500);
    assert_eq!(four.booking(TAIL_SLOT), 2000);
    assert_eq!((three.reach(TAIL_SLOT), four.reach(TAIL_SLOT)), (3, 4));
    assert_eq!(tail(0, 8).booking(TAIL_SLOT), 1500);
    assert_eq!(four.booking(63), 2000);
    assert_eq!(four.reach(63), 8);
}

/// A slot past the tail slot reads the tail slot, for the booking and for the reach.
#[test]
fn a_slot_past_the_tail_reads_the_tail() {
    let mut p = class(&[32; 128], 4);
    learn(&mut p, &[32; 130], 4);
    let c = Curve::of(&p, 1500, WIDE, E);
    assert_eq!(c.booking(TAIL_SLOT + 1), c.booking(TAIL_SLOT));
    assert_eq!(c.booking(usize::MAX), 2000);
    assert_eq!(c.reach(TAIL_SLOT + 1), 4);
    assert_eq!(c.reach(usize::MAX), 4);
}

/// Every slot is held to the clamp: a 90th percentile above the most books the most, one
/// below the least books the least, equal to either bound books it, and the flat number
/// is held to the clamp as well.
#[test]
fn every_slot_is_held_to_the_clamp() {
    let p = two_cores(4);
    let clamp = |least_millis, most_millis| Clamp {
        least_millis,
        most_millis,
    };
    let c = Curve::of(&p, 5000, clamp(250, 1800), E);
    assert_eq!((c.booking(0), c.booking(2)), (1800, 1800));
    assert_eq!((c.booking(3), c.booking(TAIL_SLOT)), (1800, 1800));
    let c = Curve::of(&p, 100, clamp(2500, 9000), E);
    assert_eq!((c.booking(0), c.booking(3)), (2500, 2500));
    assert_eq!(Curve::of(&p, 100, clamp(2000, 2000), E).booking(0), 2000);
    assert_eq!(Curve::of(&p, 100, clamp(250, 2000), E).booking(0), 2000);
    assert_eq!(Curve::of(&p, 100, clamp(250, 1999), E).booking(0), 1999);
    assert_eq!(Curve::of(&p, 100, clamp(2001, 9000), E).booking(0), 2001);
    assert_eq!(Curve::of(&p, 100, clamp(2000, 9000), E).booking(0), 2000);
}

/// Sixteenths become millicores rounded up: 1 is 62.5 and books 63, 2 is exactly 125,
/// 3 is 187.5 and books 188, 16 is exactly 1000, 17 is 1062.5 and books 1063.
#[test]
fn sixteenths_round_up_to_millicores() {
    for (v, m) in [(1, 63), (2, 125), (3, 188), (16, 1000), (17, 1063)] {
        let c = Curve::of(&class(&[v; 4], 4), 0, WIDE, E);
        assert_eq!(c.booking(0), m, "{v} sixteenths");
    }
}

/// The peak is the largest booking of any slot: a start slot's when it is the largest, the
/// tail's when that is, and the flat number's when the runs did not reach most slots.
#[test]
fn the_peak_is_the_largest_booking() {
    let p = class(&[16, 16, 48, 48], 8);
    assert_eq!(Curve::of(&p, 100, WIDE, E).peak(), 3000);
    let quiet = class(&[16; 4], 4);
    assert_eq!(Curve::of(&quiet, 4000, WIDE, E).peak(), 4000);
    let mut burst = vec![16; 128];
    burst.extend([160; 2]);
    let c = Curve::of(&class(&burst, 4), 100, WIDE, E);
    assert_eq!(c.booking(0), 1000);
    assert_eq!(c.booking(TAIL_SLOT), 10_000);
    assert_eq!(c.peak(), 10_000);
}

/// A running job is charged by survival: with 4 of 8 runs ending by 40 s and the rest by
/// 80 s, a job at age 0 is charged all of its booking while all 8 reach, half from 40 s
/// to 80 s and nothing where no run reached. A class whose runs all end by 40 s charges 0
/// past 40 s.
#[test]
fn a_running_job_is_charged_by_survival() {
    let mut p = class(&[32; 40], 4);
    learn(&mut p, &[32; 80], 4);
    let c = Curve::of(&p, 1500, WIDE, E);
    assert_eq!(c.charge(0, 0, 777), 2000);
    assert_eq!(c.charge(0, 19, 777), 2000);
    assert_eq!(c.charge(0, 20, 777), 1000);
    assert_eq!(c.charge(0, 39, 777), 1000);
    assert_eq!(c.charge(0, 40, 777), 0);
    assert_eq!(c.charge(0, 45, 777), 0);
    let all = Curve::of(&class(&[32; 40], 8), 1500, WIDE, E);
    assert_eq!(all.charge(0, 19, 777), 2000);
    assert_eq!(all.charge(0, 20, 777), 0);
    assert_eq!(all.charge(0, TAIL_SLOT, 777), 0);
}

/// A charge is rounded up: a slot of 63 m that 5 of the 8 runs at the job's age reach is
/// charged 39.375, which rounds up to 40.
#[test]
fn the_charge_rounds_up() {
    let mut p = class(&[1; 4], 3);
    learn(&mut p, &[1; 6], 5);
    let c = Curve::of(&p, 1500, WIDE, E);
    assert_eq!(c.booking(2), 63);
    assert_eq!(c.charge(0, 2, 777), 40);
    assert_eq!(c.charge(0, 1, 777), 63);
}

/// A job whose own slot fewer than 4 kept runs reached has outlived the record and is
/// charged its flat number at every slot ahead, the tail slot included; with 4 runs it is
/// weighted, and a job one slot younger sees the runs of its own slot.
#[test]
fn a_job_past_the_recorded_runs_is_charged_flat() {
    let three = Curve::of(&two_cores(3), 1500, WIDE, E);
    for k in [0, 1, 5, TAIL_SLOT, TAIL_SLOT + 7] {
        assert_eq!(three.charge(4000, k, 777), 777, "k {k}");
    }
    let four = Curve::of(&two_cores(4), 1500, WIDE, E);
    assert_eq!(four.charge(4000, 0, 777), 2000);
    assert_eq!(four.charge(4000, 1, 777), 0);
    assert_eq!(four.charge(4000, TAIL_SLOT, 777), 0);
    assert_eq!(four.charge(3999, 0, 777), 2000);
    assert_eq!(four.charge(3999, 1, 777), 1000);
    assert_eq!(four.charge(8000, 0, 777), 777);
}

/// A job past 128 s is charged the class's tail booking at every slot, unweighted; with
/// fewer than 4 runs past 128 s that booking is the clamped flat number.
#[test]
fn a_job_past_128_seconds_is_charged_the_tail() {
    let mut p = class(&[32; 128], 4);
    learn(&mut p, &[32; 130], 4);
    let c = Curve::of(&p, 1500, WIDE, E);
    for age in [128_000, 500_000] {
        for k in [0, 1, 40, TAIL_SLOT, TAIL_SLOT + 3] {
            assert_eq!(c.charge(age, k, 777), 2000, "age {age} k {k}");
        }
    }
    let thin = Curve::of(&class(&[32; 130], 3), 1500, WIDE, E);
    assert_eq!(thin.charge(200_000, 0, 777), 1500);
}

/// The slot ahead is the tail from slot 64 on, weighted by the share of the runs at the
/// job's slot that lasted past 128 s: from slot 1, 62 slots ahead is still a start slot
/// and 63 is the tail; `k` of the tail slot and beyond is the tail.
#[test]
fn a_slot_ahead_past_the_start_slots_is_the_tail() {
    let mut p = class(&[32; 128], 4);
    learn(&mut p, &[32; 130], 4);
    let c = Curve::of(&p, 1500, WIDE, E);
    assert_eq!(c.charge(0, 63, 777), 2000);
    assert_eq!(c.charge(2000, 62, 777), 2000);
    assert_eq!(c.charge(2000, 63, 777), 1000);
    assert_eq!(c.charge(126_000, 0, 777), 2000);
    assert_eq!(c.charge(126_000, 1, 777), 1000);
    assert_eq!(c.charge(0, TAIL_SLOT, 777), 1000);
    assert_eq!(c.charge(0, TAIL_SLOT + 5, 777), 1000);
}
