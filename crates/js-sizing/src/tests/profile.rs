//! The learner's building blocks: spans, weights, edges, and every statistic of a
//! profile on small series whose answers are worked by hand.

use super::*;
use crate::profile::{edges, spans, weight};

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

fn series(granularity_secs: u32, points: &[u32], run_usec: u64) -> RunSeries<'_> {
    RunSeries {
        granularity_secs,
        points,
        run_usec,
    }
}

/// A profile of `unit` that learned `points` as a clean run ending at `E`.
fn learned(unit: Unit, points: &[u32]) -> Profile {
    let mut p = Profile::new(unit);
    p.check_and_learn(pts(points), E, true);
    p
}

/// Each point covers one second and the last ends at the run's wall time when that is
/// above the point's start; below it the last point still covers one second.
#[test]
fn spans_of_one_second_points() {
    assert_eq!(
        spans(series(1, &[1, 2, 3], 3_000_000)),
        vec![(0, 1, 1), (1, 1, 2), (2, 1, 3)]
    );
    assert_eq!(
        spans(series(1, &[1, 2, 3], 1_500_000)),
        vec![(0, 1, 1), (1, 1, 2), (2, 1, 3)]
    );
}

/// With a granularity above one the last point ends at the run's wall time rounded up,
/// at least one second after its start and at most a full point.
#[test]
fn spans_clamp_the_last_point_both_ways() {
    let points = [5, 6];
    assert_eq!(spans(series(4, &points, 0)), vec![(0, 4, 5), (4, 1, 6)]);
    assert_eq!(
        spans(series(4, &points, 6_000_001)),
        vec![(0, 4, 5), (4, 3, 6)]
    );
    assert_eq!(
        spans(series(4, &points, 8_000_000)),
        vec![(0, 4, 5), (4, 4, 6)]
    );
    assert_eq!(
        spans(series(4, &points, 100_000_000)),
        vec![(0, 4, 5), (4, 4, 6)]
    );
}

/// A granularity of zero reads as one second, and a series with no point has no span.
#[test]
fn spans_read_zero_granularity_as_one_and_empty_as_none() {
    assert_eq!(
        spans(series(0, &[9, 8], 2_000_000)),
        vec![(0, 1, 9), (1, 1, 8)]
    );
    assert!(spans(series(1, &[], 5_000_000)).is_empty());
}

/// A series whose total is within 5% of the run's exact CPU time agrees, on both sides,
/// and exactly at 5% it still agrees.
#[test]
fn a_series_agrees_within_five_percent() {
    let one = series(1, &[19], 1_000_000);
    assert!(series_agrees(one, 1_250_000));
    assert!(!series_agrees(one, 1_250_001));
    let more = series(1, &[21], 1_000_000);
    assert!(series_agrees(more, 1_250_000));
    assert!(!series_agrees(more, 1_249_999));
    assert!(series_agrees(series(1, &[16, 16], 2_000_000), 2_000_000));
}

/// A zero usage agrees only with a zero total, and a series with no point has total zero.
#[test]
fn a_zero_usage_agrees_only_with_a_zero_total() {
    assert!(series_agrees(series(1, &[0], 1_000_000), 0));
    assert!(!series_agrees(series(1, &[1], 1_000_000), 0));
    assert!(!series_agrees(series(1, &[], 0), 1_000));
    assert!(series_agrees(series(1, &[], 0), 0));
}

/// A run's weight is 1 now, one half after one half-life and a quarter after two; a run
/// older than the shift can express weighs nothing.
#[test]
fn weights_halve_every_half_life() {
    assert_eq!(weight(0), 1 << 32);
    assert_eq!(weight(HALF_LIFE_MS), 1 << 31);
    assert_eq!(weight(2 * HALF_LIFE_MS), 1 << 30);
    assert_eq!(weight(33 * HALF_LIFE_MS), 0);
    assert_eq!(weight(100 * HALF_LIFE_MS), 0);
    assert_eq!(weight(u64::MAX), 0);
}

/// A fraction of a half-life multiplies the factor of each set bit of its 64ths: half a
/// half-life is the first factor alone, a 64th the last alone.
#[test]
fn weights_of_a_fraction_use_the_factor_of_each_bit() {
    assert_eq!(weight(HALF_LIFE_MS / 2), 3_037_000_500);
    assert_eq!(weight(HALF_LIFE_MS / 64), 4_248_701_965);
    let both = (3_037_000_500u128 * 4_248_701_965u128) >> 32;
    assert_eq!(
        u128::from(weight(HALF_LIFE_MS / 2 + HALF_LIFE_MS / 64)),
        both
    );
    assert!(weight(HALF_LIFE_MS - 1) > weight(HALF_LIFE_MS));
}

/// Each factor squared is the factor before it (to rounding), and the first squared is
/// one half: the factors are `2^(-1/2)` down to `2^(-1/64)`.
#[test]
fn the_fraction_factors_chain_by_squaring() {
    let factors = [
        3_037_000_500u128,
        3_611_622_603,
        3_938_502_376,
        4_112_874_773,
        4_202_935_003,
        4_248_701_965,
    ];
    let square = |f: u128| ((f * f) >> 32) as i128;
    assert!((square(factors[0]) - (1 << 31)).abs() <= 4);
    for pair in factors.windows(2) {
        assert!((square(pair[1]) - pair[0] as i128).abs() <= 4);
    }
}

/// A run that ended after `at_ms` weighs as a run that ends at `at_ms`.
#[test]
fn a_run_ending_after_the_time_asked_has_full_weight() {
    let p = learned(CPU_UNIT, &[10, 10]);
    assert_eq!(p.level(0), Some(10));
    assert_eq!(p.slot_mean(0, 0), Some(10));
}

/// A run six hours older counts half, and runs of equal age count equally.
#[test]
fn an_older_run_counts_less() {
    let t = 2 * HALF_LIFE_MS;
    let mut p = Profile::new(CPU_UNIT);
    p.check_and_learn(pts(&[40, 40]), t - HALF_LIFE_MS, true);
    p.check_and_learn(pts(&[10, 10]), t, true);
    assert_eq!(p.level(t), Some(20));
    assert_eq!(p.slot_mean(0, t), Some(20));
    assert_eq!(p.level(t + HALF_LIFE_MS), Some(20));
    assert_eq!(p.level(0), Some(25));
    let mut equal = Profile::new(CPU_UNIT);
    equal.check_and_learn(pts(&[40, 40]), t, true);
    equal.check_and_learn(pts(&[10, 10]), t, true);
    assert_eq!(equal.level(t), Some(25));
    assert_eq!(equal.slot_mean(0, t), Some(25));
}

/// Runs old enough to weigh nothing leave no level, mean, tail or percentile of all
/// samples; a slot they reached has a percentile of 0, and the runs are still kept.
#[test]
fn runs_of_no_weight_say_nothing() {
    let p = learned(CPU_UNIT, &[10, 10]);
    let late = E + 100 * HALF_LIFE_MS;
    assert_eq!(p.runs(), 1);
    assert_eq!(p.level(late), None);
    assert_eq!(p.slot_mean(0, late), None);
    assert_eq!(p.slot_p90(0, late), Some(0));
    assert_eq!(p.tail(late), None);
    assert_eq!(p.level_p90(late), None);
    assert_eq!(p.histogram(late), [0; BUCKETS]);
}

/// An empty profile answers nothing.
#[test]
fn an_empty_profile_answers_nothing() {
    let p = Profile::new(CPU_UNIT);
    assert_eq!(p.runs(), 0);
    assert_eq!(p.level(E), None);
    assert_eq!(p.slot_mean(0, E), None);
    assert_eq!(p.slot_p90(0, E), None);
    assert_eq!(p.slot_runs(0), 0);
    assert_eq!(p.tail(E), None);
    assert_eq!(p.level_p90(E), None);
    assert_eq!(p.histogram(E), [0; BUCKETS]);
    assert_eq!(p.window(), Window::default());
    assert!(!p.shaped());
}

/// A run's slots, level and counts on a 4 s run: slot 0 is the mean of 4 and 8, slot 1
/// of 6 and 2; there is no slot 2, no slot past the start slots and no tail.
#[test]
fn a_short_run_gives_its_slots_and_level() {
    let p = learned(CPU_UNIT, &[4, 8, 6, 2]);
    assert_eq!(p.slot_mean(0, E), Some(6));
    assert_eq!(p.slot_mean(1, E), Some(4));
    assert_eq!(p.slot_mean(2, E), None);
    assert_eq!(p.slot_mean(START_SLOTS, E), None);
    assert_eq!(p.slot_runs(0), 1);
    assert_eq!(p.slot_runs(1), 1);
    assert_eq!(p.slot_runs(2), 0);
    assert_eq!(p.level(E), Some(5));
    assert_eq!(p.tail(E), None);
    assert_eq!(p.slot_p90(0, E), Some(6));
    assert_eq!(p.slot_p90(2, E), None);
}

/// A slot's mean and the level round half up.
#[test]
fn means_round_half_up() {
    let p = learned(CPU_UNIT, &[1, 2]);
    assert_eq!(p.slot_mean(0, E), Some(2));
    assert_eq!(p.level(E), Some(2));
    let q = learned(CPU_UNIT, &[1, 1, 1, 2]);
    assert_eq!(q.level(E), Some(1));
    assert_eq!(q.slot_mean(1, E), Some(2));
}

/// The same series gives a CPU slot its mean and a memory slot its largest second.
#[test]
fn combine_mean_and_max_differ_on_the_same_series() {
    let points = [2, 6, 3, 3];
    let cpu = learned(CPU_UNIT, &points);
    let memory = learned(MEMORY_UNIT, &points);
    assert_eq!(cpu.slot_mean(0, E), Some(4));
    assert_eq!(memory.slot_mean(0, E), Some(6));
    assert_eq!(cpu.slot_mean(1, E), Some(3));
    assert_eq!(memory.slot_mean(1, E), Some(3));
    assert_eq!(cpu.level(E), memory.level(E));
}

/// A run of an odd number of seconds has a last slot of one second, which is that
/// second's value and counts one second in the level.
#[test]
fn an_odd_run_ends_in_a_one_second_slot() {
    let p = learned(CPU_UNIT, &[2, 6, 9]);
    assert_eq!(p.slot_mean(0, E), Some(4));
    assert_eq!(p.slot_mean(1, E), Some(9));
    assert_eq!(p.slot_p90(1, E), Some(9));
    assert_eq!(p.level(E), Some(6));
}

/// A point that straddles two slots counts its seconds in each: a point of 3 s then one
/// of 3 s put 4 and 8 in the middle slot, a mean of 6 or a largest of 8.
#[test]
fn a_point_across_slots_counts_in_each() {
    let points = [4, 8];
    let run = series(3, &points, 6_000_000);
    let mut cpu = Profile::new(CPU_UNIT);
    cpu.check_and_learn(run, E, true);
    let mut memory = Profile::new(MEMORY_UNIT);
    memory.check_and_learn(run, E, true);
    assert_eq!(cpu.slot_mean(0, E), Some(4));
    assert_eq!(cpu.slot_mean(1, E), Some(6));
    assert_eq!(cpu.slot_mean(2, E), Some(8));
    assert_eq!(memory.slot_mean(1, E), Some(8));
    assert_eq!(cpu.slot_mean(3, E), None);
    assert_eq!(cpu.level(E), Some(6));
}

/// A run past 128 s keeps 64 start slots and the mean of what follows as its tail; a
/// point that straddles 128 s counts only its later seconds in the tail.
#[test]
fn a_long_run_has_a_tail() {
    let points = [5, 7];
    let mut p = Profile::new(CPU_UNIT);
    let c = p.check_and_learn(series(100, &points, 200_000_000), E, true);
    assert_eq!(c.actual_mean, 6);
    assert_eq!(c.actual_peak, 7);
    assert_eq!(p.slot_mean(49, E), Some(5));
    assert_eq!(p.slot_mean(50, E), Some(7));
    assert_eq!(p.slot_mean(63, E), Some(7));
    assert_eq!(p.slot_mean(64, E), None);
    assert_eq!(p.slot_runs(63), 1);
    assert_eq!(p.slot_runs(64), 0);
    assert_eq!(p.tail(E), Some(7));
    assert_eq!(p.level(E), Some(6));
}

/// A run of exactly 128 s has no tail.
#[test]
fn a_run_of_128_seconds_has_no_tail() {
    let points = [3; 128];
    let p = learned(CPU_UNIT, &points);
    assert_eq!(p.tail(E), None);
    assert_eq!(p.slot_runs(63), 1);
    assert_eq!(p.level(E), Some(3));
}

/// The 90th percentile weighs a value by the load it carries: nine runs of 1 and one of
/// 100 in a slot give 100, because the one run carries most of the load.
#[test]
fn the_slot_percentile_is_weighted_by_load() {
    let mut p = Profile::new(CPU_UNIT);
    for _ in 0..9 {
        p.check_and_learn(pts(&[1, 1]), E, true);
    }
    assert_eq!(p.slot_p90(0, E), Some(1));
    p.check_and_learn(pts(&[100, 100]), E, true);
    assert_eq!(p.slot_p90(0, E), Some(100));
    assert_eq!(p.slot_p90(1, E), None);
}

/// A slot whose runs all used nothing has a percentile of 0 and a level of 0; the
/// percentile of all samples is the first bucket's edge, 1.
#[test]
fn a_slot_of_zeros_has_percentile_zero() {
    let p = learned(CPU_UNIT, &[0, 0]);
    assert_eq!(p.slot_p90(0, E), Some(0));
    assert_eq!(p.level(E), Some(0));
    assert_eq!(p.level_p90(E), Some(1));
}

/// The edges start at 1, end at the unit's top, never fall and grow geometrically; a
/// unit whose top is 1 or 0 has every edge at 1.
#[test]
fn the_edges_are_log_spaced_from_one_to_the_top() {
    for unit in [CPU_UNIT, MEMORY_UNIT] {
        let e = edges(unit);
        assert_eq!(e[0], 1);
        assert_eq!(e[BUCKETS - 1], unit.top);
        assert!(e.windows(2).all(|w| w[0] <= w[1]));
        assert!(e[BUCKETS - 2] < unit.top);
    }
    let memory = edges(MEMORY_UNIT);
    assert!((1100..=1200).contains(&memory[32]));
    for top in [0, 1] {
        let unit = Unit {
            combine: Combine::Mean,
            top,
        };
        assert_eq!(edges(unit), [1; BUCKETS]);
    }
}

/// Every second of a run lands in the bucket whose edge first reaches its value: 0 and 1
/// in the first, a value above the top in the last.
#[test]
fn the_histogram_counts_seconds_by_value() {
    let e = edges(CPU_UNIT);
    let p = learned(CPU_UNIT, &[1, 1, 1000, 1000]);
    let h = p.histogram(E);
    let b = e.partition_point(|&x| x < 1000);
    assert!(b > 0);
    assert_eq!(h[0], 2 << 32);
    assert_eq!(h[b], 2 << 32);
    assert_eq!(h.iter().sum::<u64>(), 4 << 32);
    let q = learned(CPU_UNIT, &[0, 0, 5000, 5000]);
    let h = q.histogram(E);
    assert_eq!(h[0], 2 << 32);
    assert_eq!(h[BUCKETS - 1], 2 << 32);
}

/// A run's tail seconds count at the tail's mean, one weight per second.
#[test]
fn the_histogram_counts_tail_seconds_at_the_tail_mean() {
    let e = edges(CPU_UNIT);
    let p = learned(CPU_UNIT, &[8; 130]);
    let h = p.histogram(E);
    let b = e.partition_point(|&x| x < 8);
    assert_eq!(h[b], 130 << 32);
    assert_eq!(h.iter().sum::<u64>(), 130 << 32);
}

/// The percentile of all samples is the upper edge of the bucket where the load
/// reaches 90%: the edge above 6 for a run of 6s and 4s, the top for a run above it.
#[test]
fn the_level_percentile_is_a_bucket_edge() {
    let e = edges(CPU_UNIT);
    let up = |v: u32| *e.iter().find(|&&x| x >= v).unwrap();
    assert_eq!(learned(CPU_UNIT, &[4, 8, 6, 2]).level_p90(E), Some(up(6)));
    assert_eq!(learned(CPU_UNIT, &[10, 10]).level_p90(E), Some(up(10)));
    assert_eq!(learned(CPU_UNIT, &[5000, 5000]).level_p90(E), Some(1408));
}
