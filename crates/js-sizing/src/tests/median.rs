//! `Summaries::median_memory`: what a typical recent sample of a key asks for, the figure a
//! key that stands in for another one books (a kind for a target with no run of its own).

use super::*;

fn median(s: &Summaries, k: &Key) -> Option<u64> {
    s.median_memory(k, DAY, &Bounds::default())
}

/// Rule: with no sample there is no answer; one sample answers with it plus the margin
/// (`1.2 x` plus 128 MiB, rounded up to 256 MiB), and at least 1 GiB.
#[test]
fn median_memory_is_none_without_a_sample_and_one_sample_answers_with_the_margin() {
    let mut s = Summaries::default();
    let k = linux(1);
    assert_eq!(median(&s, &k), None);
    feed(&mut s, &k, &success(3 * GIB), 1);
    // 3 GiB x 1.2 + 128 MiB = 3.725 GiB, rounded up to 3.75 GiB.
    assert_eq!(median(&s, &k), Some(15 * 256 * MIB));
    let mut small = Summaries::default();
    feed(&mut small, &k, &success(100 * MIB), 1);
    assert_eq!(median(&small, &k), Some(MIN_MEMORY));
}

/// Rule: the statistic is the median, not the high end. The farm's case: a kind whose runs
/// peak at 0.27 GiB, with two runs of a heavy member at 7 GiB among its last 32, books what
/// its high end asks for (8.75 GiB) by `peak_memory`, and 1 GiB by `median_memory`. Catches
/// a figure that follows the kind's heaviest member.
#[test]
fn heavy_samples_among_light_ones_do_not_move_the_median() {
    let mut s = Summaries::default();
    let k = linux(1);
    let light = 270 * MIB;
    let heavy = 7 * GIB;
    feed(&mut s, &k, &success(light), 15);
    feed(&mut s, &k, &success(heavy), 2);
    feed(&mut s, &k, &success(light), 15);
    assert_eq!(
        s.peak_memory(&k, DAY, &Bounds::default()),
        Some(35 * 256 * MIB)
    );
    assert_eq!(median(&s, &k), Some(GIB));
}

/// Rule: the median is the middle sample, the larger of the two middle ones for an even
/// count, so two samples answer with the larger and three with the middle one. Catches a
/// lower median, which books a kind of two runs at its smaller one.
#[test]
fn the_median_of_an_even_count_is_the_larger_middle_sample() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 1);
    feed(&mut s, &k, &success(3 * GIB), 1);
    assert_eq!(median(&s, &k), Some(15 * 256 * MIB), "two: the larger");
    feed(&mut s, &k, &success(2 * GIB), 1);
    // 2 GiB x 1.2 + 128 MiB = 2.525 GiB, rounded up to 2.75 GiB.
    assert_eq!(median(&s, &k), Some(11 * 256 * MIB), "three: the middle");
}

/// Rule: the median is over the last 32 samples: a kind whose work grew follows it within
/// 32 runs, and one whose heavy runs are older than 32 runs no longer books them. Catches a
/// median over the whole history, or over fewer runs than were kept.
#[test]
fn the_median_follows_the_last_32_samples() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 40);
    assert_eq!(median(&s, &k), Some(GIB));
    feed(&mut s, &k, &success(4 * GIB), 16);
    // 16 of the last 32 at 4 GiB: the larger middle sample is 4 GiB: 4.8 GiB + 128 MiB.
    assert_eq!(median(&s, &k), Some(20 * 256 * MIB));
    feed(&mut s, &k, &success(500 * MIB), 17);
    assert_eq!(
        median(&s, &k),
        Some(GIB),
        "the 4 GiB runs are a minority again"
    );
}

/// Rule: held samples (a run that reached its booking) count as recorded, the larger of
/// their peak and booking; the answer is the larger of the fresh levels; a stale key has
/// none; and a key with no configuration answers from its one level.
#[test]
fn the_median_counts_held_samples_takes_the_larger_level_and_is_none_when_stale() {
    let mut s = Summaries::default();
    let at_limit = Run {
        limit_hits: 1,
        booked_memory_bytes: 3 * GIB,
        ..success(GIB)
    };
    feed(&mut s, &linux(1), &at_limit, 1);
    assert_eq!(
        median(&s, &linux(1)),
        Some(15 * 256 * MIB),
        "the held booking"
    );

    let mut s = Summaries::default();
    feed(&mut s, &linux(1), &success(500 * MIB), 3);
    feed(&mut s, &linux(2), &success(3 * GIB), 4);
    // L1 of config 1 is 1 GiB; L2 holds 3 small and 4 large runs: 3.75 GiB.
    assert_eq!(median(&s, &linux(1)), Some(15 * 256 * MIB));
    let later = Day(DAY.0 + STALE_DAYS + 1);
    assert_eq!(s.median_memory(&linux(1), later, &Bounds::default()), None);

    let lone = key(Basis::Cgroup, None);
    let mut t = Summaries::default();
    assert_eq!(median(&t, &lone), None);
    feed(&mut t, &lone, &success(2 * GIB), 1);
    assert_eq!(median(&t, &lone), Some(11 * 256 * MIB));
}
