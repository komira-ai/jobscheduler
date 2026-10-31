//! Welch intervals, the permutation cross-check and the std/mean comparison.

use crate::describe::{mean, summarize, variance};
use crate::quantile::t_critical;
use crate::types::{Comparison, CvComparison, Interval, Pair, PermMethod, Permutation, Verdict};

/// The 0.975 quantile of the standard normal distribution.
const NORMAL_975: f64 = 1.959_963_984_540_054;
/// Splits above which the permutation test samples instead of enumerating.
const EXACT_LIMIT: u128 = 1_000_000;
/// Splits drawn when sampling.
const SAMPLES: u64 = 100_000;
/// The fixed seed of the sampling.
const SEED: u64 = 0x2545_F491_4F6C_DD1D;
/// Values per arm below which a comparison is underpowered.
const MIN_RUNS: usize = 5;
/// Values per arm below which a std/mean comparison is underpowered.
const MIN_CV_RUNS: usize = 10;

/// The Welch-Satterthwaite degrees of freedom of two squared standard errors; none when
/// both are zero.
fn welch_df(v0: f64, n0: usize, v1: f64, n1: usize) -> Option<f64> {
    let s = v0 + v1;
    (s > 0.0).then(|| s * s / (v0 * v0 / ((n0 - 1) as f64) + v1 * v1 / ((n1 - 1) as f64)))
}

/// The half-width of the 95% interval of a quantity with squared standard error `se2`.
fn half_width(se2: f64, df: Option<f64>) -> f64 {
    df.map_or(0.0, |d| t_critical(d) * se2.sqrt())
}

/// Compares `arm` with `base`: Welch's 95% interval of the difference of means, the
/// relative change and the permutation cross-check.
#[must_use]
pub fn compare(setting: &str, metric: &str, base: Pair<'_>, arm: Pair<'_>) -> Comparison {
    let (n0, n1) = (base.values.len(), arm.values.len());
    let mut c = Comparison {
        setting: setting.to_string(),
        metric: metric.to_string(),
        base: base.name.to_string(),
        arm: arm.name.to_string(),
        n_base: n0,
        n_arm: n1,
        diff: None,
        diff_ci: None,
        df: None,
        rel_change: None,
        rel_ci: None,
        permutation: None,
        verdict: Verdict::NotEstimable,
        underpowered: true,
    };
    if n0 == 0 || n1 == 0 {
        return c;
    }
    let (m0, m1) = (mean(base.values), mean(arm.values));
    let diff = m1 - m0;
    c.diff = Some(diff);
    c.permutation = Some(permutation_test(base.values, arm.values));
    if n0 < 2 || n1 < 2 {
        return c;
    }
    let v0 = variance(base.values) / n0 as f64;
    let v1 = variance(arm.values) / n1 as f64;
    let df = welch_df(v0, n0, v1, n1);
    let hw = half_width(v0 + v1, df);
    c.df = df;
    c.diff_ci = Some(Interval {
        lo: diff - hw,
        hi: diff + hw,
    });
    c.verdict = if diff - hw > 0.0 || diff + hw < 0.0 {
        Verdict::Differs
    } else {
        Verdict::NotDistinguished
    };
    c.underpowered = n0 < MIN_RUNS || n1 < MIN_RUNS || hw > diff.abs();
    if m0 > 0.0 && m1 > 0.0 {
        let l0 = v0 / (m0 * m0);
        let l1 = v1 / (m1 * m1);
        let hw_log = half_width(l0 + l1, welch_df(l0, n0, l1, n1));
        let log_ratio = (m1 / m0).ln();
        c.rel_change = Some(m1 / m0 - 1.0);
        c.rel_ci = Some(Interval {
            lo: (log_ratio - hw_log).exp() - 1.0,
            hi: (log_ratio + hw_log).exp() - 1.0,
        });
    }
    c
}

/// std/mean of `values` when it is defined and positive.
fn positive_cv(values: &[f64]) -> Option<f64> {
    summarize(values).and_then(|s| s.cv).filter(|c| *c > 0.0)
}

/// Compares the std/mean of `arm` with that of `base`: the log of the ratio of the two is
/// taken as normal with variance 1/(2(n-1)) + cv^2/n per arm, and the arms differ when its
/// 95% interval excludes zero. A large-sample rule: below 10 values per arm it is flagged.
#[must_use]
pub fn cv_compare(setting: &str, metric: &str, base: Pair<'_>, arm: Pair<'_>) -> CvComparison {
    let (n0, n1) = (base.values.len(), arm.values.len());
    let (cv0, cv1) = (positive_cv(base.values), positive_cv(arm.values));
    let mut c = CvComparison {
        setting: setting.to_string(),
        metric: metric.to_string(),
        base: base.name.to_string(),
        arm: arm.name.to_string(),
        n_base: n0,
        n_arm: n1,
        cv_base: cv0,
        cv_arm: cv1,
        ratio: None,
        ratio_ci: None,
        verdict: Verdict::NotEstimable,
        underpowered: true,
    };
    let (Some(v0), Some(v1)) = (cv0, cv1) else {
        return c;
    };
    let var = |v: f64, n: usize| 1.0 / (2.0 * (n - 1) as f64) + v * v / n as f64;
    let half = NORMAL_975 * (var(v0, n0) + var(v1, n1)).sqrt();
    let log_ratio = (v1 / v0).ln();
    c.ratio = Some(v1 / v0);
    c.ratio_ci = Some(Interval {
        lo: (log_ratio - half).exp(),
        hi: (log_ratio + half).exp(),
    });
    c.verdict = if log_ratio - half > 0.0 || log_ratio + half < 0.0 {
        Verdict::Differs
    } else {
        Verdict::NotDistinguished
    };
    c.underpowered = n0 < MIN_CV_RUNS || n1 < MIN_CV_RUNS;
    c
}

/// A small, fixed generator (SplitMix64) for the sampled permutation test.
struct SplitMix(u64);

impl SplitMix {
    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The pooled values and what a split needs to be judged.
struct Pool {
    values: Vec<f64>,
    total: f64,
    n0: usize,
    n1: usize,
    threshold: f64,
}

impl Pool {
    /// Whether the split whose first set sums to `sum0` differs at least as much as the
    /// observed one (a relative tolerance absorbs summation order).
    fn extreme(&self, sum0: f64) -> bool {
        ((self.total - sum0) / self.n1 as f64 - sum0 / self.n0 as f64).abs() >= self.threshold
    }

    fn walk(&self, start: usize, need: usize, sum: f64, hits: &mut u64) {
        if need == 0 {
            if self.extreme(sum) {
                *hits += 1;
            }
            return;
        }
        let room = self.values.len() - need + 1;
        for (i, v) in self.values.iter().enumerate().take(room).skip(start) {
            self.walk(i + 1, need - 1, sum + v, hits);
        }
    }
}

/// C(n, k) for small k, saturating when it is far above any limit used here.
fn binomial(n: usize, k: usize) -> u128 {
    let mut r: u128 = 1;
    for i in 1..=k {
        r = r.saturating_mul((n - k + i) as u128) / i as u128;
    }
    r
}

/// The two-sided permutation test of the difference of means of `arm` and `base`. Up to
/// 1,000,000 splits are all enumerated (exact); above that 100,000 random splits are drawn
/// from a fixed seed and `p = (hits + 1) / (draws + 1)`.
#[must_use]
pub fn permutation_test(base: &[f64], arm: &[f64]) -> Permutation {
    let (n0, n1) = (base.len(), arm.len());
    let values: Vec<f64> = base.iter().chain(arm).copied().collect();
    let observed = mean(arm) - mean(base);
    let scale = values.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    let pool = Pool {
        total: values.iter().sum(),
        values,
        n0,
        n1,
        threshold: observed.abs() - 1e-9 * scale,
    };
    let splits = binomial(n0 + n1, n0.min(n1));
    let mut hits = 0_u64;
    if splits <= EXACT_LIMIT {
        pool.walk(0, n0, 0.0, &mut hits);
        return Permutation {
            p: hits as f64 / splits as f64,
            method: PermMethod::Exact,
            splits: splits as u64,
        };
    }
    let mut rng = SplitMix(SEED);
    let mut order = pool.values.clone();
    for _ in 0..SAMPLES {
        for i in (1..order.len()).rev() {
            let j = (rng.draw() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        if pool.extreme(order[..n0].iter().sum()) {
            hits += 1;
        }
    }
    Permutation {
        p: (hits + 1) as f64 / (SAMPLES + 1) as f64,
        method: PermMethod::MonteCarlo,
        splits: SAMPLES,
    }
}
