//! The Welch rule: the interval of the difference of means, its flags and the relative change.

use js_analysis::{Comparison, Interval, Pair, PermMethod, Verdict, compare, summarize};

fn close(got: f64, want: f64, tolerance: f64) {
    assert!((got - want).abs() <= tolerance, "got {got}, want {want}");
}

fn run(base: &[f64], arm: &[f64]) -> Comparison {
    let base = Pair {
        name: "base",
        values: base,
    };
    let arm = Pair {
        name: "arm",
        values: arm,
    };
    compare("s", "m", base, arm)
}

fn ci(c: &Comparison) -> Interval {
    c.diff_ci.unwrap()
}

/// Asserts Welch's test on Student's sleep data (R's `sleep` data set, group 1 against
/// group 2). R's `t.test(extra ~ group, data = sleep)` documents: t = -1.8608, df = 17.776,
/// p = 0.07939, 95% interval -3.3654832 to 0.2054832 for mean(group 1) - mean(group 2);
/// here the arm is group 2, so the difference and the interval change sign.
#[test]
fn sleep_data_matches_the_published_welch_test() {
    let group1 = [0.7, -1.6, -0.2, -1.2, -0.1, 3.4, 3.7, 0.8, 0.0, 2.0];
    let group2 = [1.9, 0.8, 1.1, 0.1, -0.1, 4.4, 5.5, 1.6, 4.6, 3.4];
    let c = run(&group1, &group2);
    close(c.diff.unwrap(), 1.58, 1e-9);
    close(c.df.unwrap(), 17.776, 1e-3);
    close(ci(&c).lo, -0.2054832, 1e-4);
    close(ci(&c).hi, 3.3654832, 1e-4);
    assert_eq!(c.verdict, Verdict::NotDistinguished);
    // The half-width 1.785 exceeds the difference 1.58.
    assert!(c.underpowered);
    let permutation = c.permutation.unwrap();
    assert_eq!(
        (permutation.method, permutation.splits),
        (PermMethod::Exact, 184_756)
    );
}

/// Asserts a case worked by hand: 1,2,3 against 4,5,6 has variance 1 each, squared
/// standard errors 1/3 each, df = (2/3)^2 / (2 (1/3)^2 / 2) = 4, and the half-width is
/// 2.776 * sqrt(2/3) = 2.267, so the interval is 3 +- 2.267.
#[test]
fn hand_worked_example() {
    let c = run(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]);
    close(c.df.unwrap(), 4.0, 1e-9);
    close(c.diff.unwrap(), 3.0, 1e-12);
    close(ci(&c).lo, 0.7330, 2e-3);
    close(ci(&c).hi, 5.2670, 2e-3);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(c.underpowered, "3 values per arm is below 5");
}

/// Asserts the arms differ although their own 95% intervals of the mean overlap: the
/// interval of the difference is the rule, not the overlap of the two intervals.
#[test]
fn arms_differ_although_their_intervals_overlap() {
    let (base, arm) = ([9.0, 10.0, 11.0], [12.0, 13.0, 14.0]);
    let a = summarize(&base).unwrap().mean_ci.unwrap();
    let b = summarize(&arm).unwrap().mean_ci.unwrap();
    assert!(b.lo < a.hi, "the per-arm intervals overlap");
    let c = run(&base, &arm);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(ci(&c).lo > 0.0);
}

/// Asserts a lower arm differs on the negative side.
#[test]
fn a_lower_arm_differs_below_zero() {
    let c = run(&[4.0, 5.0, 6.0], &[1.0, 2.0, 3.0]);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(ci(&c).hi < 0.0 && c.diff.unwrap() < 0.0);
}

/// Asserts an interval that contains zero does not tell the arms apart.
#[test]
fn an_interval_with_zero_does_not_distinguish() {
    let c = run(&[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0]);
    assert_eq!(c.verdict, Verdict::NotDistinguished);
    assert!(ci(&c).lo < 0.0 && ci(&c).hi > 0.0);
}

/// Asserts 5 against 5 values that differ clearly are not underpowered (df 8, half-width
/// 2.306 against a difference of 10).
#[test]
fn five_against_five_that_differ_are_not_underpowered() {
    let c = run(
        &[10.0, 11.0, 12.0, 13.0, 14.0],
        &[20.0, 21.0, 22.0, 23.0, 24.0],
    );
    assert_eq!(c.verdict, Verdict::Differs);
    close(c.df.unwrap(), 8.0, 1e-9);
    assert!(!c.underpowered);
}

/// Asserts 5 against 5 values whose half-width exceeds the difference are underpowered.
#[test]
fn a_half_width_above_the_difference_is_underpowered() {
    let c = run(
        &[10.0, 12.0, 14.0, 16.0, 18.0],
        &[11.0, 13.0, 15.0, 17.0, 19.0],
    );
    assert_eq!(c.verdict, Verdict::NotDistinguished);
    assert!(c.underpowered);
}

/// Asserts fewer than 5 values on the arm side flags a comparison that does differ.
#[test]
fn fewer_than_five_on_the_arm_side_is_underpowered() {
    let c = run(&[10.0, 11.0, 12.0, 13.0, 14.0], &[20.0, 21.0, 22.0]);
    assert_eq!(c.verdict, Verdict::Differs);
    assert!(c.underpowered);
}

/// Asserts a single value on either side gives the difference and the permutation test but
/// no interval and no verdict.
#[test]
fn one_value_cannot_be_estimated() {
    for (base, arm) in [
        (&[5.0][..], &[1.0, 2.0, 3.0][..]),
        (&[1.0, 2.0, 3.0][..], &[5.0][..]),
    ] {
        let c = run(base, arm);
        assert!(c.diff.is_some() && c.permutation.is_some());
        assert!(c.diff_ci.is_none() && c.df.is_none() && c.rel_ci.is_none());
        assert_eq!(c.verdict, Verdict::NotEstimable);
        assert!(c.underpowered);
    }
}

/// Asserts a side with no value gives nothing at all: no difference, no test.
#[test]
fn no_values_give_nothing() {
    for (base, arm) in [
        (&[][..], &[1.0, 2.0, 3.0][..]),
        (&[1.0, 2.0, 3.0][..], &[][..]),
    ] {
        let c = run(base, arm);
        assert!(c.diff.is_none() && c.permutation.is_none() && c.diff_ci.is_none());
        assert_eq!(c.verdict, Verdict::NotEstimable);
    }
}

/// Asserts that when both variances are zero there are no degrees of freedom, the interval
/// is the point difference, and a nonzero difference differs.
#[test]
fn zero_variance_gives_a_point_interval() {
    let c = run(&[5.0, 5.0, 5.0], &[7.0, 7.0, 7.0]);
    assert!(c.df.is_none());
    assert_eq!(ci(&c), Interval { lo: 2.0, hi: 2.0 });
    assert_eq!(c.verdict, Verdict::Differs);
    close(c.rel_change.unwrap(), 0.4, 1e-12);
    close(c.rel_ci.unwrap().lo, 0.4, 1e-12);
    let same = run(&[5.0, 5.0, 5.0], &[5.0, 5.0, 5.0]);
    assert_eq!(same.verdict, Verdict::NotDistinguished);
}

/// Asserts the relative change and its delta-method interval on the log of the ratio:
/// 10 against 20 with squared standard errors 1/3 and 4/3 gives log-scale squared standard
/// errors 1/300 each, df 4, half-width 2.776 * 0.08165 = 0.2267 on ln 2 = 0.6931, so the
/// change is +100% with the interval exp(0.4664) - 1 = 0.594 to exp(0.9198) - 1 = 1.509.
#[test]
fn relative_change_interval_by_the_delta_method() {
    let c = run(&[9.0, 10.0, 11.0], &[18.0, 20.0, 22.0]);
    close(c.rel_change.unwrap(), 1.0, 1e-12);
    close(c.rel_ci.unwrap().lo, 0.594, 5e-3);
    close(c.rel_ci.unwrap().hi, 1.509, 5e-3);
}

/// Asserts no relative change when either mean is not positive, while the difference
/// interval is still given.
#[test]
fn no_relative_change_for_a_mean_that_is_not_positive() {
    let zero_base = run(&[-1.0, 0.0, 1.0], &[3.0, 4.0, 5.0]);
    assert!(zero_base.diff_ci.is_some() && zero_base.rel_change.is_none());
    let zero_arm = run(&[3.0, 4.0, 5.0], &[-1.0, 0.0, 1.0]);
    assert!(zero_arm.diff_ci.is_some() && zero_arm.rel_change.is_none());
}
