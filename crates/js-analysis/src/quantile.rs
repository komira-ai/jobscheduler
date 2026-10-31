//! Quantiles of Student's t and chi-square, computed from the regularised incomplete beta
//! and gamma functions and inverted by bisection. No tables: the results agree with closed
//! forms and published values to at least eight significant digits (pinned in the tests).

use std::f64::consts::PI;

/// Lanczos coefficients (g = 7, n = 9).
const LANCZOS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    9.984_369_578_019_572e-6,
    1.505_632_735_149_311_6e-7,
];

/// Relative size below which a series term no longer changes the sum.
const SERIES_EPS: f64 = 1e-17;

/// ln of the gamma function for `x >= 0.5`.
fn ln_gamma(x: f64) -> f64 {
    let z = x - 1.0;
    let mut a = LANCZOS[0];
    for (i, c) in LANCZOS.iter().enumerate().skip(1) {
        a += c / (z + i as f64);
    }
    let t = z + 7.5;
    0.5 * (2.0 * PI).ln() + (z + 0.5) * t.ln() - t + a.ln()
}

/// The series sum_n (a+b)_n / (a+1)_n x^n of the incomplete beta function.
fn beta_series(x: f64, a: f64, b: f64) -> f64 {
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut n = 0.0;
    while term > SERIES_EPS * sum {
        term *= (a + b + n) / (a + 1.0 + n) * x;
        sum += term;
        n += 1.0;
    }
    sum
}

/// The regularised incomplete beta function I_x(a, b) for `0 < x < 1`.
fn inc_beta(x: f64, a: f64, b: f64) -> f64 {
    let front =
        (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_series(x, a, b) / a
    } else {
        1.0 - front * beta_series(1.0 - x, b, a) / b
    }
}

/// The regularised lower incomplete gamma function P(a, x) for `x > 0`.
fn gamma_p(a: f64, x: f64) -> f64 {
    let mut term = 1.0 / a;
    let mut sum = term;
    let mut k = 1.0;
    while term > SERIES_EPS * sum {
        term *= x / (a + k);
        sum += term;
        k += 1.0;
    }
    sum * (a * x.ln() - x - ln_gamma(a)).exp()
}

/// P(T > t) for Student's t with `df` degrees of freedom and `t > 0`.
fn t_upper_tail(t: f64, df: f64) -> f64 {
    0.5 * inc_beta(df / (df + t * t), df / 2.0, 0.5)
}

/// The `p` quantile of Student's t with `df >= 1` degrees of freedom (fractional allowed),
/// for `0.5 < p < 1`.
#[must_use]
pub fn t_quantile(p: f64, df: f64) -> f64 {
    let target = 1.0 - p;
    let (mut lo, mut hi) = (0.0, 1e6);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if t_upper_tail(mid, df) > target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The two-sided 95% critical value of Student's t: the 0.975 quantile.
#[must_use]
pub fn t_critical(df: f64) -> f64 {
    t_quantile(0.975, df)
}

/// The `p` quantile of the chi-square distribution with `df >= 1` degrees of freedom,
/// for `0 < p < 1`.
#[must_use]
pub fn chi_square_quantile(p: f64, df: f64) -> f64 {
    // Far above the 0.975 quantile (about df + 2 sqrt(2 df)), and low enough that the gamma
    // series below stays within the range of an f64 for any df of a benchmark.
    let (mut lo, mut hi) = (0.0, df + 10.0 * df.sqrt() + 50.0);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if gamma_p(df / 2.0, mid / 2.0) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}
