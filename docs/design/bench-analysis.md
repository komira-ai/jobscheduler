# Benchmark analysis (`js-analysis`)

This is the statistics every benchmark arm shares. A measurement crate plans, drives and
records runs; it emits one JSON line per run and leaves all statistics to `js-analysis`
(`crates/js-analysis`). The crate reads the lines, groups them, and reports the numbers and the
rules below. It reads no clock and no environment variable; the one random draw (a sampled
permutation test) uses a fixed seed, so the same input always gives the same report.

## Input: one record per run

JSON lines, one object per line, blank lines ignored, unknown keys refused. The Rust type is
`RunRecord` (`src/types.rs`):

```rust
pub struct RunRecord {
    pub setting: String,   // the varied parameter's value as text, e.g. "0.5"
    pub arm: String,       // what was run, e.g. "legacy + MinIO"
    pub run_index: u32,    // unique within (setting, arm)
    pub seed: u64,
    pub commit: String,    // the build under test
    #[serde(default)]
    pub label: String,     // "warmup" for a warm-up run, anything else is a measured run
    pub metrics: BTreeMap<String, Option<f64>>, // null = not available
}
```

```json
{"setting":"0.5","arm":"tierstore","run_index":3,"seed":42,"commit":"abc1234","label":"","metrics":{"queue_wait_s":12.5,"makespan_s":null}}
```

- **Group** = (setting, arm), in order of first appearance; runs inside a group keep input order.
- **Every run appears** in the output (JSON `groups[].runs`, and the markdown "Runs" table).
  Nothing is dropped, trimmed or reordered.
- **Warm-ups are labelled, not removed.** A run labelled `warmup` is listed, counted
  (`n_warmup`), and left out of every statistic. Any other label (or none) is a measured run.
- **Not available is explicit.** A metric whose value is `null` (or absent from the record) is
  excluded from that metric's statistics and counted in `n_missing`, per group and metric. It is
  never read as zero, never `NaN`, never imputed. A metric with no value at all has no summary.
- A repeated (setting, arm, run_index) is an error. A named base arm that has no run is an error.

## Per group and metric: n, missing and the summary

With the measured, available values x1..xn: `n`, `n_missing`, mean, median, standard deviation
(sample, divisor n-1), min, max, std/mean (`cv`), and:

- **95% interval of the mean** (needs n >= 2): `mean +- t * std / sqrt(n)` with
  `t = t(0.975, n-1)`. The quantile is computed (regularised incomplete beta, inverted by
  bisection), not looked up; it matches the tables: df 2 gives 4.303, 4 gives 2.776, 10 gives
  2.228, 30 gives 2.042.
- **95% interval of std/mean** (needs n >= 2 and a positive mean): the modified McKay
  approximation, a closed form, no resampling. With v = std/mean, df = n-1 and
  `u_hi`, `u_lo` the 0.975 and 0.025 quantiles of chi-square(df):

  `lower = v / sqrt(((u_hi + 2)/n - 1) v^2 + u_hi/df)`,
  `upper = v / sqrt(((u_lo + 2)/n - 1) v^2 + u_lo/df)`.

  It assumes roughly normal values. When the upper denominator is not positive (a large
  std/mean at small n, e.g. 0.5 at n = 3) the approximation has no upper end and no interval is
  reported rather than a made-up one.

n = 1 has a mean, median, min and max and nothing else. A mean that is not positive has no
std/mean.

## When two arms differ (the rule)

Two arms differ on a metric at a setting when **the 95% Welch confidence interval for the
difference of their means excludes zero.**

With arms 0 (base) and 1, `d = mean1 - mean0`, `v_i = s_i^2 / n_i`:

- `se = sqrt(v0 + v1)`
- Welch-Satterthwaite degrees of freedom `df = (v0 + v1)^2 / (v0^2/(n0-1) + v1^2/(n1-1))`
- interval `d +- t(0.975, df) * se`, with the fractional df used as it is.

The verdict is `differs` if the interval excludes zero, `not-distinguished` if it contains
zero, and `not-estimable` if a side has fewer than two values. When both variances are zero
there is no df; the interval is the point `d`.

**Not distinguished is not "equal".** It says the data cannot tell the arms apart at this n.

**Do not decide by overlap of the two per-arm intervals.** Overlap of two 95% intervals is too
conservative (two intervals can overlap while the interval of the difference excludes zero) and
is commonly misread as "no difference". The rule is the interval of the difference, only. The
worked example below overlaps and differs.

### Reported with the verdict

- `diff`, its interval, `df`.
- **Relative change** `mean1/mean0 - 1` with its interval **by the delta method on the log of
  the ratio of means**: `ln(mean1/mean0) +- t * sqrt(l0 + l1)`, `l_i = v_i / mean_i^2`, df by
  Welch-Satterthwaite applied to `l0` and `l1`, then back-transformed with `exp(.) - 1`. It is
  defined when both means are positive. It is a report, not the rule: the verdict comes from the
  interval of the difference.
- **Permutation cross-check**: the exact two-sided permutation test of the difference of means.
  All C(n0+n1, n0) splits of the pooled values are enumerated, up to 1,000,000 splits;
  `p` = share of splits whose |difference| is at least the observed (a relative 1e-9 tolerance
  absorbs summation order). Above the limit, 100,000 random splits are drawn from a fixed seed
  and `p = (hits+1)/(draws+1)`; the report says `exact` or `monte-carlo` and the split count.
- **At n = 3 per arm the permutation test cannot reach 0.05**: there are C(6,3) = 20 splits, and
  the most extreme result has itself and its mirror image as extreme, so the smallest attainable p
  is 2/20 = 0.1. (Four per arm: 2/70 = 0.029; five: 2/252 = 0.008.) That is why the Welch interval is
  the rule and the permutation p is a cross-check, not a gate.
- **`underpowered`** is set when either side has fewer than 5 values, or when the half-width of
  the interval exceeds the difference. The second condition is the same as "the interval
  contains zero", so every `not-distinguished` result is flagged; it marks a null result that
  carries little information. A `differs` result at n < 5 is flagged too: the interval relies on
  a variance estimated from very few runs.

### Worked example

Base `9, 10, 11` and arm `12, 13, 14` (3 runs each).

- mean 10 and 13, `s^2` = 1 each, `v` = 1/3 each, `d` = 3, `se` = sqrt(2/3) = 0.8165.
- `df = (2/3)^2 / (2 * (1/3)^2 / 2) = 4`, `t(0.975, 4) = 2.776`, half-width 2.267.
- Interval for the difference: 3 +- 2.267 = **[0.733, 5.267]**: excludes zero, so the arms differ.
- Per-arm 95% intervals of the mean: 10 +- 2.484 = [7.516, 12.484] and [10.516, 15.484]. They
  overlap, and the arms still differ: overlap is not the rule.
- Relative change +30%, delta-method interval [+5.6%, +60.0%] (df 3.75, t 2.850).
- Permutation: 20 splits, p = 0.1 (the floor at n = 3). Underpowered: yes (n < 5).

## Choosing n: the reproducibility check

Run **the same arm n times** (the reproducibility check). Its group report gives the spread:
std/mean with its interval, and the range (min, max). The spread is what decides how many runs
an arm comparison needs; do this before the comparison runs, on the baseline arm.

For two arms with the same std/mean `v`, n runs each, the expected half-width of the 95% Welch
interval of the *relative* difference is about `t(0.975, 2(n-1)) * v * sqrt(2/n)`. The report
gives, for each requested relative effect `e` (`--effect`, default 5% and 10%):

- **`min_runs_ci`**: the smallest n >= 3 with `t(0.975, 2(n-1)) * v * sqrt(2/n) <= e`. At this n
  the interval just resolves an effect of size e (about 50% chance to see it).
- **`min_runs_power80`**: the smallest n with `(t(0.975, 2(n-1)) + t(0.8, 2(n-1))) * v *
  sqrt(2/n) <= e`: about 80% power to see a true effect of e.

The search stops at 200 runs per arm (`>200` in the table). Planning table, equal spread (ci /
power80):

| std/mean | effect 2% | effect 5% | effect 10% |
|---|---|---|---|
| 1% | 4 / 6 | 3 / 3 | 3 / 3 |
| 2% | 9 / 17 | 3 / 4 | 3 / 3 |
| 5% | 50 / 100 | 9 / 17 | 4 / 6 |
| 10% | 194 / >200 | 32 / 64 | 9 / 17 |

Use the std/mean the baseline showed (take the upper end of its interval if n is small), pick
the smallest effect worth resolving, and run at least the power80 count. Fewer than 5 runs per
arm is always flagged. The arms table that needs n >= 3 per arm resolves only large effects
unless the spread is around 2% or lower.

## Comparing two arms' std/mean (shared vs whole host)

For the noise question (the same benchmark n times on a shared host against n times on a whole
host), `--compare-cv BASE ARM` compares std/mean. Rule: the arms differ in spread when the 95%
interval of `ln(cv1/cv0)` excludes zero, where `ln(cv_i)` is taken as normal with variance
`1/(2(n_i - 1)) + cv_i^2 / n_i` (the delta method on `ln std` and `ln mean` of normal values) and
the interval uses the normal quantile 1.960. Reported: both std/mean values, the ratio
`cv1/cv0` and its interval `exp(ln ratio +- 1.960 * se)`. It is a large-sample rule: fewer than 10
values on either side is flagged `underpowered`, and it is `not-estimable` when a std/mean is not
defined or is zero. With 20 runs per side it is the intended use. The per-arm interval of
std/mean (McKay, above) is for reading one arm; as with means, the comparison is never decided
by overlap of the two per-arm intervals.

## Command line

```sh
js-analysis RUNS.jsonl [--baseline ARM | --ladder] [--compare-cv BASE ARM]... \
    [--effect 0.05]... [--json OUT.json]
```

Markdown goes to standard output (a table per metric; the comparisons; the std/mean
comparisons; the run counts; every run); `--json` writes the same report as JSON. Comparisons
are within a setting: every other arm against the base (`--baseline`, default the first arm of
the setting), or each arm against the one before it (`--ladder`).

## Tests

`crates/js-analysis/tests`: quantiles against table values (df 2, 4, 10, 30 and more) and chi-square
table values; Welch against the published `t.test` result for Student's sleep data (R's `sleep`
data set: df 17.776, interval -3.3654832 to 0.2054832) and against a hand-worked example;
permutation enumeration counts (20, 252, 924, 705,432 splits), the 0.1 floor, the sampled
case and its repeatability; grouping, warm-ups, not-available values; the markdown and JSON
output; the command line and the program.
