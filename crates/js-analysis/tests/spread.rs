//! The comparison of two arms' std/mean.

use js_analysis::{CvComparison, Pair, Verdict, cv_compare};

fn run(base: &[f64], arm: &[f64]) -> CvComparison {
    let base = Pair {
        name: "shared",
        values: base,
    };
    let arm = Pair {
        name: "whole host",
        values: arm,
    };
    cv_compare("s", "m", base, arm)
}

/// `n` values around 100 whose deviations cycle through -2, -1, 0, 1, 2 times 3 times `scale`.
fn around(n: i32, scale: f64) -> Vec<f64> {
    (0..n)
        .map(|i| 100.0 + f64::from(i % 5 - 2) * 3.0 * scale)
        .collect()
}

/// Asserts that 20 runs with four times the std/mean differ: std/mean 0.0435 against 0.1741,
/// ratio 4, and the interval of the log of the ratio (half-width 0.456 around ln 4 = 1.386)
/// excludes zero.
#[test]
fn a_fourfold_spread_with_twenty_runs_differs() {
    let c = run(&around(20, 1.0), &around(20, 4.0));
    assert!((c.ratio.unwrap() - 4.0).abs() < 1e-9);
    assert!(c.ratio_ci.unwrap().lo > 1.0);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(!c.underpowered);
}

/// Asserts equal spreads do not differ.
#[test]
fn equal_spreads_do_not_differ() {
    let c = run(&around(20, 1.0), &around(20, 1.0));
    assert_eq!(c.verdict, Verdict::NotDistinguished);
    assert!((c.ratio.unwrap() - 1.0).abs() < 1e-9);
}

/// Asserts a lower std/mean on the arm side differs below one, and that fewer than 10 runs
/// is flagged.
#[test]
fn a_smaller_spread_differs_below_one_and_few_runs_are_flagged() {
    let c = run(&around(20, 4.0), &around(20, 1.0));
    assert!(c.ratio_ci.unwrap().hi < 1.0);
    assert_eq!(c.verdict, Verdict::Differs);
    let few = run(&[9.0, 10.0, 11.0], &[18.0, 20.0, 22.0]);
    assert!(few.underpowered);
    assert!(run(&around(20, 1.0), &[9.0, 10.0, 11.0]).underpowered);
    assert_eq!(few.verdict, Verdict::NotDistinguished);
}

/// Asserts no verdict when a std/mean is not defined or is zero.
#[test]
fn no_verdict_without_a_positive_spread() {
    let flat = run(&[5.0, 5.0, 5.0], &[1.0, 2.0, 3.0]);
    assert!(flat.cv_base.is_none() && flat.cv_arm.is_some() && flat.ratio.is_none());
    assert_eq!(flat.verdict, Verdict::NotEstimable);
    let single = run(&[1.0, 2.0, 3.0], &[4.0]);
    assert!(single.cv_arm.is_none());
    assert_eq!(single.verdict, Verdict::NotEstimable);
}

/// Asserts the ends of the ratio interval against values worked by hand. Twenty values with
/// deviations of 0, +-3 and +-6 have std/mean 0.043530; with 0, +-12 and +-24 they have 0.174120.
/// The variance of ln(std/mean) is 1/(2 * 19) + cv^2/20 = 0.026411 and 0.027832, so
/// se = sqrt(0.054243) = 0.232900, the half-width on the log scale is 1.959964 * 0.232900 = 0.456480,
/// and the ratio 4 has the interval exp(ln 4 -+ 0.456480) = [2.5341, 6.3140]; the other way
/// round, 0.25 has [0.158378, 0.394625].
#[test]
fn the_ratio_interval_ends_are_pinned() {
    let up = run(&around(20, 1.0), &around(20, 4.0));
    let ci = up.ratio_ci.unwrap();
    assert!((ci.lo - 2.534_053).abs() < 1e-5, "{}", ci.lo);
    assert!((ci.hi - 6.313_996).abs() < 1e-5, "{}", ci.hi);
    let down = run(&around(20, 4.0), &around(20, 1.0));
    let ci = down.ratio_ci.unwrap();
    assert!((down.ratio.unwrap() - 0.25).abs() < 1e-9);
    assert!((ci.lo - 0.158_378).abs() < 1e-5, "{}", ci.lo);
    assert!((ci.hi - 0.394_625).abs() < 1e-5, "{}", ci.hi);
}

/// Asserts the underpowered boundary: 10 against 10 is not flagged, 9 against 10 and 10
/// against 9 are.
#[test]
fn ten_runs_per_side_is_the_boundary() {
    assert!(!run(&around(10, 1.0), &around(10, 4.0)).underpowered);
    assert!(run(&around(9, 1.0), &around(10, 4.0)).underpowered);
    assert!(run(&around(10, 1.0), &around(9, 4.0)).underpowered);
}
