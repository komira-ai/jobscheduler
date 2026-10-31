//! The permutation test: enumeration counts, attainable p-values and the sampled case.

use js_analysis::{PermMethod, permutation_test};

fn close(got: f64, want: f64) {
    assert!((got - want).abs() <= 1e-12, "got {got}, want {want}");
}

/// Asserts 3 against 3 enumerates 20 splits and the smallest p is 2/20 = 0.1: it cannot
/// reach 0.05 however far apart the arms are.
#[test]
fn three_against_three_cannot_go_below_a_tenth() {
    let t = permutation_test(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]);
    assert_eq!((t.method, t.splits), (PermMethod::Exact, 20));
    close(t.p, 0.1);
}

/// Asserts the split counts are the binomial coefficients: C(10,5), C(5,2), C(12,6).
#[test]
fn split_counts_are_binomial() {
    let separated = |n0: usize, n1: usize| {
        let base: Vec<f64> = (0..n0).map(|i| i as f64).collect();
        let arm: Vec<f64> = (0..n1).map(|i| 1000.0 + i as f64).collect();
        permutation_test(&base, &arm)
    };
    assert_eq!(separated(5, 5).splits, 252);
    assert_eq!(separated(2, 3).splits, 10);
    assert_eq!(separated(6, 6).splits, 924);
    assert_eq!(separated(1, 4).splits, 5);
}

/// Asserts the smallest p of 5 against 5 is 2/252 and of 2 against 3 is 1/10 (the mirror
/// split of unequal sizes has a smaller difference).
#[test]
fn smallest_attainable_p_values() {
    let base5: Vec<f64> = (0..5).map(f64::from).collect();
    let arm5: Vec<f64> = (10..15).map(f64::from).collect();
    close(permutation_test(&base5, &arm5).p, 2.0 / 252.0);
    close(permutation_test(&[1.0, 2.0], &[10.0, 11.0, 12.0]).p, 0.1);
}

/// Asserts that ties up to rounding count as equal: 0.1..0.3 against 0.4..0.6 has the
/// same two extreme splits as 1..3 against 4..6.
#[test]
fn rounding_does_not_break_ties() {
    close(permutation_test(&[0.1, 0.2, 0.3], &[0.4, 0.5, 0.6]).p, 0.1);
}

/// Asserts identical values give p = 1.
#[test]
fn identical_values_give_one() {
    close(permutation_test(&[2.0, 2.0, 2.0], &[2.0, 2.0, 2.0]).p, 1.0);
}

/// Asserts a mixed sample is not extreme: interleaved values give a large p.
#[test]
fn interleaved_values_give_a_large_p() {
    let t = permutation_test(&[1.0, 4.0, 5.0, 8.0], &[2.0, 3.0, 6.0, 7.0]);
    assert_eq!(t.splits, 70);
    assert!(t.p > 0.5);
}

/// Asserts the enumeration limit: 11 against 11 (705,432 splits) is enumerated.
#[test]
fn eleven_against_eleven_is_enumerated() {
    let base: Vec<f64> = (0..11).map(f64::from).collect();
    let arm: Vec<f64> = (100..111).map(f64::from).collect();
    let t = permutation_test(&base, &arm);
    assert_eq!((t.method, t.splits), (PermMethod::Exact, 705_432));
    close(t.p, 2.0 / 705_432.0);
}

/// Asserts above the limit (12 against 12 has 2,704,156 splits) 100,000 splits are drawn,
/// and that a clearly separated pair gets a p near 1/100,001.
#[test]
fn twelve_against_twelve_is_sampled() {
    let base: Vec<f64> = (1..=12).map(f64::from).collect();
    let arm: Vec<f64> = (101..=112).map(f64::from).collect();
    let t = permutation_test(&base, &arm);
    assert_eq!((t.method, t.splits), (PermMethod::MonteCarlo, 100_000));
    assert!(t.p > 0.0 && t.p < 1e-4, "{}", t.p);
}

/// Asserts the sampled test is repeatable (fixed seed) and not extreme for interleaved arms.
#[test]
fn sampling_is_repeatable() {
    let base: Vec<f64> = (0..20).map(|i| f64::from(2 * i + 1)).collect();
    let arm: Vec<f64> = (0..20).map(|i| f64::from(2 * i + 2)).collect();
    let first = permutation_test(&base, &arm);
    assert_eq!(first, permutation_test(&base, &arm));
    assert_eq!(first.method, PermMethod::MonteCarlo);
    assert!(first.p > 0.5, "{}", first.p);
}

/// Asserts the sampled p of one case to its exact value, so that any change of the generator,
/// the seed, the draw count or the shuffle shows: 0..11 against 3..14 has 6321 of the 100,000
/// draws at least as extreme as the observed difference, so p = 6322 / 100001.
#[test]
fn the_sampled_p_is_pinned() {
    let base: Vec<f64> = (0..12).map(f64::from).collect();
    let arm: Vec<f64> = (3..15).map(f64::from).collect();
    let t = permutation_test(&base, &arm);
    assert_eq!(t.method, PermMethod::MonteCarlo);
    close(t.p, 6322.0 / 100_001.0);
}

/// Asserts the tie tolerance does not depend on the offset of the data: the same 3 against 3
/// shifted by a billion still has p = 0.1, where a tolerance of 1e-9 of the largest value
/// would count splits a whole unit apart as tied.
#[test]
fn a_large_offset_does_not_blur_ties() {
    let shifted = |v: [f64; 3]| v.map(|x| x + 1e9);
    let t = permutation_test(&shifted([1.0, 2.0, 3.0]), &shifted([4.0, 5.0, 6.0]));
    close(t.p, 0.1);
}
