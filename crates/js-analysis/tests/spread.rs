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

/// Twenty values around 100 whose deviations are +-6, +-3 and 0 times `scale`.
fn around_100(scale: f64) -> Vec<f64> {
    (0..20)
        .map(|i| 100.0 + f64::from(i % 5 - 2) * 3.0 * scale)
        .collect()
}

/// Asserts that 20 runs with four times the std/mean differ: std/mean 0.0435 against 0.1741,
/// ratio 4, and the interval of the log of the ratio (half-width 0.456 around ln 4 = 1.386)
/// excludes zero.
#[test]
fn a_fourfold_spread_with_twenty_runs_differs() {
    let c = run(&around_100(1.0), &around_100(4.0));
    assert!((c.ratio.unwrap() - 4.0).abs() < 1e-9);
    assert!(c.ratio_ci.unwrap().lo > 1.0);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(!c.underpowered);
}

/// Asserts equal spreads do not differ.
#[test]
fn equal_spreads_do_not_differ() {
    let c = run(&around_100(1.0), &around_100(1.0));
    assert_eq!(c.verdict, Verdict::NotDistinguished);
    assert!((c.ratio.unwrap() - 1.0).abs() < 1e-9);
}

/// Asserts a lower std/mean on the arm side differs below one, and that fewer than 10 runs
/// is flagged.
#[test]
fn a_smaller_spread_differs_below_one_and_few_runs_are_flagged() {
    let c = run(&around_100(4.0), &around_100(1.0));
    assert!(c.ratio_ci.unwrap().hi < 1.0);
    assert_eq!(c.verdict, Verdict::Differs);
    let few = run(&[9.0, 10.0, 11.0], &[18.0, 20.0, 22.0]);
    assert!(few.underpowered);
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
