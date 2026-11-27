//! The memory curve of a class: the high end of its runs' peaks after 128 s, when a class has
//! one, what each slot holds with the margin, and what a running job is still projected to hold.

use super::*;

/// When the runs of these tests end, and when every profile is read: each run weighs the same.
const E: u64 = 10 * HALF_LIFE_MS;

/// A series of one-second points in MiB whose run lasts exactly its points.
fn pts(points: &[u32]) -> RunSeries<'_> {
    RunSeries {
        granularity_secs: 1,
        points,
        run_usec: points.len() as u64 * 1_000_000,
    }
}

/// Feeds `points` to `p` as `n` clean runs ending at `E`.
fn learn(p: &mut Profile, points: &[u32], n: usize) {
    for _ in 0..n {
        p.check_and_learn(pts(points), E, true);
    }
}

/// A memory profile that learned `n` runs of `points`.
fn class(points: &[u32], n: usize) -> Profile {
    let mut p = Profile::new(MEMORY_UNIT);
    learn(&mut p, points, n);
    p
}

/// A run of `secs` seconds at `low` MiB for its first `rise` seconds, then at `high`.
fn rising(secs: usize, rise: usize, (low, high): (u32, u32)) -> Vec<u32> {
    let mut points = vec![low; secs];
    points[rise..].fill(high);
    points
}

/// `mib` with the margin of the flat memory booking, in bytes: `1.2 x mib + 128 MiB`.
fn margined(mib: u64) -> u64 {
    (mib + mib.div_ceil(5) + 128) * MIB
}

/// Asserts that the tail's 90th percentile is that of the runs' peaks after 128 s, weighted by
/// their load, and not their mean: nine runs peak at 100 MiB after 128 s and one at 1000 MiB,
/// whose load is more than a tenth of all, so the 90th percentile is 1000 while the tail mean is
/// 190; a class whose runs end before 128 s has none, and one whose tails hold nothing has 0.
/// Catches a tail read as a mean, an unweighted percentile, and a run with no tail counted.
#[test]
fn the_tail_p90_is_the_high_end_of_the_runs_peaks_after_128_s() {
    let mut p = class(&rising(140, 128, (50, 100)), 9);
    learn(&mut p, &rising(140, 128, (50, 1000)), 1);
    learn(&mut p, &[700; 20], 1);
    assert_eq!(p.tail_p90(E), Some(1000));
    assert_eq!(p.tail(E), Some(190));
    assert_eq!(class(&[700; 20], 3).tail_p90(E), None);
    assert_eq!(class(&rising(140, 128, (50, 0)), 3).tail_p90(E), Some(0));
}

/// Asserts that a class has no memory curve until its profile has scored 16 runs: 23 runs (15
/// scored, the first eight are not) give none and 24 give one. Catches a curve from too little
/// history, which must book flat.
#[test]
fn a_class_with_fewer_than_sixteen_scored_runs_has_no_memory_curve() {
    let points = rising(20, 10, (1000, 3000));
    let mut p = class(&points, 23);
    assert_eq!(MemoryCurve::of(&p, 4 * GIB, E), None);
    learn(&mut p, &points, 1);
    assert!(MemoryCurve::of(&p, 4 * GIB, E).is_some());
}

/// Asserts what each slot holds: a class that holds 1000 MiB for 10 s, then 3000 MiB until its
/// 20th second, holds `1.2 x p90 + 128 MiB` at each slot its runs reached (1328 MiB, then 3728
/// MiB) but never more than its flat booking (3 GiB caps the second), and the flat booking at
/// every slot no run reached and from 128 s on; a slot past the tail reads the tail. Catches a
/// slot with no margin, one above the flat booking, and a slot no run reached read as 0.
#[test]
fn each_slot_holds_its_p90_with_the_margin_and_never_more_than_flat() {
    let p = class(&rising(20, 10, (1000, 3000)), 24);
    let curve = MemoryCurve::of(&p, 4 * GIB, E).unwrap();
    assert_eq!(curve.booking(0), margined(1000));
    assert_eq!(curve.booking(4), margined(1000));
    assert_eq!(curve.booking(5), margined(3000));
    assert_eq!(curve.booking(10), 4 * GIB);
    assert_eq!(curve.booking(TAIL_SLOT), 4 * GIB);
    assert_eq!(curve.booking(TAIL_SLOT + 5), 4 * GIB);
    let capped = MemoryCurve::of(&p, 3 * GIB, E).unwrap();
    assert_eq!(capped.booking(4), margined(1000));
    assert_eq!(capped.booking(5), 3 * GIB);
}

/// Asserts what a running job is projected to hold: 4 s into a class whose runs rise at 10 s and
/// end at 20 s, it holds its own slot's 1328 MiB now, the 3728 MiB of the slot it will be in
/// three slots ahead, and nothing once every run of its class had ended (eight slots ahead, and
/// from 128 s on). Catches a projection that holds the flat booking to the end, one that reads
/// the slot of the job's age instead of the slot ahead, and one weighted by survival.
#[test]
fn a_running_job_holds_the_slots_ahead_and_nothing_once_every_run_has_ended() {
    let p = class(&rising(20, 10, (1000, 3000)), 24);
    let curve = MemoryCurve::of(&p, 4 * GIB, E).unwrap();
    assert_eq!(curve.charge(4_000, 0), margined(1000));
    assert_eq!(curve.charge(4_000, 3), margined(3000));
    assert_eq!(curve.charge(4_000, 8), 0);
    assert_eq!(curve.charge(4_000, TAIL_SLOT), 0);
}

/// Asserts that a slot few runs reached holds the flat booking: of 24 runs ending at 20 s, three
/// more last to 30 s, so a job 4 s old is projected at its flat booking 8 slots ahead (the 21st
/// second, which three runs reached) and at nothing 13 slots ahead (no run reached the 31st).
/// Catches a slot that fewer than four runs reached read as a curve, or as an end.
#[test]
fn a_slot_few_runs_reached_holds_the_flat_booking() {
    let mut p = class(&rising(20, 10, (1000, 3000)), 24);
    learn(&mut p, &rising(30, 10, (1000, 3000)), 3);
    let curve = MemoryCurve::of(&p, 4 * GIB, E).unwrap();
    assert_eq!(curve.charge(4_000, 8), 4 * GIB);
    assert_eq!(curve.charge(4_000, 13), 0);
}

/// Asserts what a job past its class's runs holds: 30 s into a class whose runs end at 20 s, it
/// has outlived the recorded runs and holds the flat booking at every slot, now and from 128 s
/// on. Catches a job that outlived its class projected to hold nothing.
#[test]
fn a_job_that_outlived_its_class_holds_the_flat_booking() {
    let p = class(&rising(20, 10, (1000, 3000)), 24);
    let curve = MemoryCurve::of(&p, 4 * GIB, E).unwrap();
    assert_eq!(curve.charge(30_000, 0), 4 * GIB);
    assert_eq!(curve.charge(30_000, TAIL_SLOT), 4 * GIB);
}

/// Asserts what a job in its tail holds: past 128 s, a job of a class whose runs link at 130 s
/// (2000 MiB until then, 8000 MiB after) holds its tail's 90th percentile with the margin (9728
/// MiB) at every slot ahead, and a job 100 s old holds that from the tail slot on. Catches a
/// tail read as its mean, which would project a link below what it holds.
#[test]
fn a_job_in_its_tail_holds_the_high_end_of_the_tails() {
    let p = class(&rising(140, 130, (2000, 8000)), 24);
    let curve = MemoryCurve::of(&p, 16 * GIB, E).unwrap();
    assert_eq!(curve.charge(200_000, 0), margined(8000));
    assert_eq!(curve.charge(200_000, 7), margined(8000));
    assert_eq!(curve.charge(100_000, 0), margined(2000));
    assert_eq!(curve.charge(100_000, TAIL_SLOT), margined(8000));
}
