//! The state of a class's four checks and the curve it books under [`Curves::On`]
//! ([`JobSizes::curve_of`], [`JobSizes::curve_counts`]): runs of the job tables on the
//! in-memory fake, each with the series its host recorded; every clock is the test's.

use js_sizing::{TAIL_SLOT, age_slot};
use js_types::sched::FarmTime;
use js_types::sched::Resources as Capacity;

use super::Curves;
use crate::engine::rig::Tables;
use crate::sizes::curve_rig::{burst_at, failed, queued_of, series_run, teach, without_series};
use crate::sizes::tests::{GIB, booked};
use crate::sizes::{JobSizes, bounds_of};

/// The farm time every curve is read at.
fn now() -> FarmTime {
    FarmTime::from_millis(5_000_000)
}

/// A history in mode `curves` over one host of 88 cores (a class books at most 22 of them).
fn sizes_in(curves: Curves) -> JobSizes {
    let mut sizes = JobSizes::new().with_curves(curves);
    sizes.set_bounds(bounds_of([Capacity::new(88_000, 256 * GIB)]));
    sizes
}

/// The bursting class every test teaches: 22 cores in each of the four seconds from the 20th, of
/// a run of 140 s.
fn bursting() -> Vec<u16> {
    burst_at(20, 352, 140)
}

/// Asserts the curve a class that passes its four checks books under `on`: its 90th percentile
/// per 2 s slot held to a quarter core and to a quarter of the host (22 cores of 88) at the two
/// slots the burst is in (the seconds 20 to 23), a quarter core in every other slot and in the
/// slot from 128 s on, which no run's CPU rises in. Catches a curve read in any other unit, one
/// not held to the least booking, and one that books the slots of another age.
#[test]
fn a_class_that_passes_its_four_checks_books_its_curve_on_the_slots_of_its_burst() {
    let tables = Tables::new();
    let mut sizes = sizes_in(Curves::On);
    teach(&mut sizes, &tables, ("burst", 24), &bursting());
    let job = queued_of(&tables, "next", "burst");
    let key = JobSizes::key_of(&job);
    let curve = sizes
        .curve_of(&key, 750, now())
        .expect("it passes its checks");
    assert_eq!(age_slot(20_000), 10);
    let booked: Vec<u64> = [0, 9, 10, 11, 12, 63, TAIL_SLOT]
        .iter()
        .map(|slot| curve.booking(*slot))
        .collect();
    assert_eq!(booked, [250, 250, 22_000, 22_000, 250, 250, 250]);
    assert_eq!(curve.peak(), 22_000);
}

/// Asserts when a class has no curve: under [`Curves::Off`] (it keeps no profile), before any
/// host is known, for a class with no profile, for one with fewer than the 16 scored runs (23
/// runs score 15), and for one whose curve predicts no better than its level (check 2); the same
/// class with 24 runs has its curve under [`Curves::Shadow`], which only judges by it, and
/// [`Curves::On`], which books it. Catches a curve kept with no profiles, with no bounds, or while
/// a check fails, and a shadow that cannot judge by the curve `on` would book.
#[test]
fn a_curve_is_known_outside_off_for_a_class_that_passes_and_with_bounds_known() {
    let tables = Tables::new();
    let job = queued_of(&tables, "next", "burst");
    let key = JobSizes::key_of(&job);
    let mut booked = Vec::new();
    for curves in [Curves::Off, Curves::Shadow, Curves::On] {
        let mut sizes = sizes_in(curves);
        teach(&mut sizes, &Tables::new(), ("burst", 24), &bursting());
        booked.push(sizes.curve_of(&key, 750, now()).is_some());
    }
    assert_eq!(booked, [false, true, true]);

    let mut unbounded = JobSizes::new().with_curves(Curves::On);
    teach(&mut unbounded, &Tables::new(), ("burst", 24), &bursting());
    assert!(
        unbounded.curve_of(&key, 750, now()).is_none(),
        "no host is known"
    );

    let mut sizes = sizes_in(Curves::On);
    let taught = Tables::new();
    teach(&mut sizes, &taught, ("burst", 23), &bursting());
    assert!(sizes.curve_of(&key, 750, now()).is_none(), "15 scored runs");
    let last = series_run(&taught, ("burst-23", "burst"), (&bursting(), 2));
    assert!(sizes.observe(&last, booked_by()));
    assert!(sizes.curve_of(&key, 750, now()).is_some(), "16 scored runs");

    let flat = [16_u16, 32].map(|level| vec![level; 10]);
    for at in 0..24 {
        let run = series_run(&tables, (&format!("flat-{at}"), "flat"), (&flat[at % 2], 1));
        assert!(sizes.observe(&run, booked_by()));
    }
    let flat_job = queued_of(&tables, "n3", "flat");
    assert!(
        sizes
            .curve_of(&JobSizes::key_of(&flat_job), 750, now())
            .is_none(),
        "not shaped"
    );
    let unknown = queued_of(&tables, "n4", "unknown");
    assert!(
        sizes
            .curve_of(&JobSizes::key_of(&unknown), 750, now())
            .is_none(),
        "no profile"
    );
}

/// What every run of this module booked.
fn booked_by() -> js_jobs::Booked {
    booked(2000, 4 * GIB)
}

/// Asserts check 4 on its boundary: a class whose last 16 runs held 12 usable series (4 runs
/// that recorded none after its 24 good ones) books its curve, and one with 11 (a fifth) books
/// flat; a run that failed pushes the same unusable run for a class that exists, so five failed
/// runs fail it too; and a class that fails the check passes again once good runs push the
/// unusable ones out of the window of 16. Catches a window of another size, a threshold off by
/// one, and a failed run not counted against the series.
#[test]
fn a_class_with_11_usable_series_of_its_last_16_books_flat_and_with_12_its_curve() {
    let tables = Tables::new();
    let mut sizes = sizes_in(Curves::On);
    teach(&mut sizes, &tables, ("burst", 24), &bursting());
    let key = JobSizes::key_of(&queued_of(&tables, "next", "burst"));
    for at in 0..4 {
        let run = without_series(&tables, &format!("none-{at}"), "burst");
        assert!(sizes.observe(&run, booked_by()));
    }
    assert!(sizes.curve_of(&key, 750, now()).is_some(), "12 of 16");
    let fifth = without_series(&tables, "none-4", "burst");
    assert!(sizes.observe(&fifth, booked_by()));
    assert!(sizes.curve_of(&key, 750, now()).is_none(), "11 of 16");
    for at in 0..12 {
        let run = series_run(&tables, (&format!("good-{at}"), "burst"), (&bursting(), 2));
        assert!(sizes.observe(&run, booked_by()));
        let again = sizes.curve_of(&key, 750, now()).is_some();
        assert_eq!(
            again,
            at == 11,
            "after {} good runs, the 5 unusable still in it",
            at + 1
        );
    }

    let mut failing = sizes_in(Curves::On);
    teach(&mut failing, &tables, ("burstf", 24), &bursting());
    let key = JobSizes::key_of(&queued_of(&tables, "nextf", "burstf"));
    for at in 0..4 {
        let run = failed(&tables, &format!("fail-{at}"), "burstf");
        let _ = failing.observe(&run, booked_by());
    }
    assert!(failing.curve_of(&key, 750, now()).is_some(), "12 of 16");
    let run = failed(&tables, "fail-4", "burstf");
    let _ = failing.observe(&run, booked_by());
    assert!(failing.curve_of(&key, 750, now()).is_none(), "11 of 16");
}

/// Asserts that a run that is no success starts no class (a failed run of a class never seen
/// leaves the profiles as they are) and counts against the series of a class that exists.
/// Catches a failure that makes a class (and may evict a learned one).
#[test]
fn a_failed_run_of_a_class_never_seen_starts_no_class() {
    let tables = Tables::new();
    let mut sizes = sizes_in(Curves::On);
    let run = failed(&tables, "fail-0", "never");
    let _ = sizes.observe(&run, booked_by());
    assert_eq!(sizes.curve_counts(), (0, 0));
}

/// Asserts the counts `/metrics` reads: classes passing all four checks and the others with a
/// profile (a class of 3 runs and one that is not shaped), and none when the profiles are not
/// kept. Catches a count that includes a class with no profile, or that counts under `off`.
#[test]
fn the_classes_are_counted_by_whether_they_pass_their_checks() {
    let mut counts = Vec::new();
    for curves in [Curves::Off, Curves::On] {
        let (mut sizes, tables) = (sizes_in(curves), Tables::new());
        assert_eq!(sizes.curve_counts(), (0, 0));
        teach(&mut sizes, &tables, ("burst", 24), &bursting());
        teach(&mut sizes, &tables, ("young", 3), &bursting());
        let flat = [16_u16, 32].map(|level| vec![level; 10]);
        for at in 0..24 {
            let run = series_run(&tables, (&format!("flat-{at}"), "flat"), (&flat[at % 2], 1));
            assert!(sizes.observe(&run, booked_by()));
        }
        counts.push(sizes.curve_counts());
    }
    assert_eq!(counts, [(0, 0), (1, 2)]);
}
