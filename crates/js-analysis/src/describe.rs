//! Summaries of one set of values, the interval of std/mean and the run-count plan.

use crate::quantile::{chi_square_quantile, t_critical, t_quantile};
use crate::types::{Interval, MAX_PLAN_RUNS, Summary};

/// The arithmetic mean of a non-empty slice.
pub(crate) fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// The sample variance (divisor n-1) of a slice of at least two values.
pub(crate) fn variance(values: &[f64]) -> f64 {
    let m = mean(values);
    values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (values.len() - 1) as f64
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// The summary of `values`; none for an empty slice.
#[must_use]
pub fn summarize(values: &[f64]) -> Option<Summary> {
    let n = values.len();
    if n == 0 {
        return None;
    }
    let m = mean(values);
    let std = (n >= 2).then(|| variance(values).sqrt());
    let cv = match std {
        Some(s) if m > 0.0 => Some(s / m),
        _ => None,
    };
    let mean_ci = std.map(|s| {
        let half = t_critical((n - 1) as f64) * s / (n as f64).sqrt();
        Interval {
            lo: m - half,
            hi: m + half,
        }
    });
    Some(Summary {
        mean: m,
        median: median(values),
        std,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        cv,
        mean_ci,
        cv_ci: cv.and_then(|v| cv_interval(v, n)),
    })
}

/// The 95% interval of the coefficient of variation `cv` (std/mean) of `n >= 2` values of a
/// normal sample, by the modified McKay approximation. None when the
/// approximation has no upper end, which happens for a large `cv` at small `n`.
#[must_use]
pub fn cv_interval(cv: f64, n: usize) -> Option<Interval> {
    let nf = n as f64;
    let df = nf - 1.0;
    let denominator = |u: f64| ((u + 2.0) / nf - 1.0) * cv * cv + u / df;
    let lo = cv / denominator(chi_square_quantile(0.975, df)).sqrt();
    let d_hi = denominator(chi_square_quantile(0.025, df));
    (d_hi > 0.0).then(|| Interval {
        lo,
        hi: cv / d_hi.sqrt(),
    })
}

/// The smallest `n >= 3` runs per arm, up to [`MAX_PLAN_RUNS`], at which two arms with
/// the same `cv` (std/mean) are expected to show a relative difference of `effect`:
/// the expected 95% Welch half-width is at most `effect` (`power80` false), or `effect`
/// is at least the half-width plus the 80% quantile of t, about 80% power (`power80`
/// true). Equal n, equal spread, `2(n-1)` degrees of freedom.
#[must_use]
pub fn min_runs(cv: f64, effect: f64, power80: bool) -> Option<usize> {
    (3..=MAX_PLAN_RUNS).find(|&n| {
        let df = 2.0 * (n as f64 - 1.0);
        let mut margin = t_quantile(0.975, df);
        if power80 {
            margin += t_quantile(0.8, df);
        }
        margin * cv * (2.0 / n as f64).sqrt() <= effect
    })
}
