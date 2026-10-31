//! The per-arm summary, the interval of std/mean and the run-count plan.

use js_analysis::{Interval, cv_interval, min_runs, summarize};

fn close(got: f64, want: f64, tolerance: f64) {
    assert!((got - want).abs() <= tolerance, "got {got}, want {want}");
}

/// Asserts every field of the summary of a textbook sample: mean 5, sum of squares 32.
#[test]
fn summary_of_a_textbook_sample() {
    let s = summarize(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
    close(s.mean, 5.0, 1e-12);
    close(s.median, 4.5, 1e-12);
    close(s.std.unwrap(), (32.0_f64 / 7.0).sqrt(), 1e-12);
    close(s.min, 2.0, 0.0);
    close(s.max, 9.0, 0.0);
    close(s.cv.unwrap(), (32.0_f64 / 7.0).sqrt() / 5.0, 1e-12);
    // t(7) = 2.365; half-width = 2.3646 * 2.1381 / sqrt(8) = 1.7875.
    let ci = s.mean_ci.unwrap();
    close(ci.lo, 3.2125, 2e-3);
    close(ci.hi, 6.7875, 2e-3);
    assert!(s.cv_ci.unwrap().lo < s.cv.unwrap());
}

/// Asserts the median of an odd count is the middle value of the sorted values.
#[test]
fn median_of_an_odd_count() {
    close(summarize(&[3.0, 1.0, 2.0]).unwrap().median, 2.0, 0.0);
}

/// Asserts a single value has no spread and no interval.
#[test]
fn one_value_has_no_spread() {
    let s = summarize(&[7.0]).unwrap();
    assert_eq!((s.mean, s.min, s.max, s.median), (7.0, 7.0, 7.0, 7.0));
    assert!(s.std.is_none() && s.cv.is_none() && s.mean_ci.is_none() && s.cv_ci.is_none());
}

/// Asserts nothing is made up for no values.
#[test]
fn no_values_have_no_summary() {
    assert!(summarize(&[]).is_none());
}

/// Asserts std/mean is not defined for a mean that is not positive.
#[test]
fn cv_needs_a_positive_mean() {
    let s = summarize(&[-1.0, 0.0, 1.0]).unwrap();
    assert!(s.std.is_some());
    assert!(s.cv.is_none() && s.cv_ci.is_none());
}

/// Asserts identical values give zero spread and a degenerate interval.
#[test]
fn identical_values_have_zero_spread() {
    let s = summarize(&[5.0, 5.0, 5.0]).unwrap();
    assert_eq!(s.std, Some(0.0));
    assert_eq!(s.cv, Some(0.0));
    assert_eq!(s.cv_ci, Some(Interval { lo: 0.0, hi: 0.0 }));
    assert_eq!(s.mean_ci, Some(Interval { lo: 5.0, hi: 5.0 }));
}

/// Asserts the modified McKay interval for std/mean 0.1 from 3 values, worked by hand from
/// the chi-square(2) quantiles 7.3778 and 0.05064: lower 0.1/sqrt((9.3778/3 - 1) 0.01 +
/// 7.3778/2) = 0.0519, upper 0.1/sqrt((2.05064/3 - 1) 0.01 + 0.05064/2) = 0.6719.
#[test]
fn cv_interval_for_three_values() {
    let ci = cv_interval(0.1, 3).unwrap();
    close(ci.lo, 0.0519, 2e-3);
    close(ci.hi, 0.6719, 2e-3);
}

/// Asserts the interval for std/mean 0.1 from 30 values, worked by hand from the
/// chi-square(29) quantiles 45.722 and 16.047: [0.0795, 0.1349].
#[test]
fn cv_interval_for_thirty_values() {
    let ci = cv_interval(0.1, 30).unwrap();
    close(ci.lo, 0.0795, 1e-3);
    close(ci.hi, 0.1349, 1e-3);
}

/// Asserts the interval narrows with more values and always contains the estimate.
#[test]
fn cv_interval_narrows_with_n() {
    let width = |n| {
        let ci = cv_interval(0.1, n).unwrap();
        assert!(ci.lo < 0.1 && 0.1 < ci.hi);
        ci.hi - ci.lo
    };
    assert!(width(3) > width(10) && width(10) > width(30));
}

/// Asserts the approximation reports no interval when its upper end does not exist
/// (std/mean 0.5 from 3 values) rather than a made-up one.
#[test]
fn cv_interval_is_none_without_an_upper_end() {
    assert!(cv_interval(0.5, 3).is_none());
}

/// Asserts the run counts at 5% spread: with 4 runs the expected 95% half-width of the
/// difference is 2.447 * 0.05 * sqrt(2/4) = 8.6% (at most 10%), with 3 it is 11.3%; about
/// 80% power for a 10% effect needs 6.
#[test]
fn run_counts_at_five_percent_spread() {
    assert_eq!(min_runs(0.05, 0.10, false), Some(4));
    assert_eq!(min_runs(0.05, 0.10, true), Some(6));
}

/// Asserts a larger spread needs more runs and a hopeless one reports none.
#[test]
fn run_counts_grow_with_spread() {
    let low = min_runs(0.05, 0.10, false).unwrap();
    let high = min_runs(0.10, 0.10, false).unwrap();
    assert!(high > low);
    assert_eq!(min_runs(0.5, 0.01, false), None);
}
