//! The learned profile of a class of runs: what its runs used over their first two
//! minutes, and on average after that.
//!
//! A [`Profile`] keeps the last [`MAX_RUNS`] runs of one class as compact records. Every
//! statistic is computed from those records when it is asked for, with each run weighted
//! by the age of its end (it halves every [`HALF_LIFE_MS`]). The profile checks each new
//! run against itself before it learns the run, and keeps the last [`CHECK_RUNS`] checks
//! as its self-check window. All arithmetic is integer, so every architecture agrees.

use std::collections::VecDeque;

/// A profile slot is this many seconds of a run's age.
pub const SLOT_SECS: u32 = 2;
/// Slots of ages 0 to 128 s.
pub const START_SLOTS: usize = 64;
/// A profile keeps at most its class's last this many runs.
pub const MAX_RUNS: usize = 64;
/// A run's weight halves every 6 h of age, by its end.
pub const HALF_LIFE_MS: u64 = 21_600_000;
/// With fewer kept runs the class is flat and no run is scored.
pub const MIN_RUNS: usize = 8;
/// A slot that fewer runs reached predicts the level.
pub const MIN_SLOT_RUNS: usize = 4;
/// The self-check's window of scored runs.
pub const CHECK_RUNS: usize = 16;
/// A class is shaped when the curve's error is at most this per mille of flat's.
pub const FIT_PERMILLE: u64 = 800;
/// The histogram's log-spaced buckets.
pub const BUCKETS: usize = 64;

/// A slot's length in seconds.
const SLOT: u64 = SLOT_SECS as u64;
/// Seconds of a run's age that the start slots cover; the tail is what follows.
const START_SECS: u64 = SLOT * START_SLOTS as u64;
/// 1.0 in Q32 fixed point.
const ONE: u64 = 1 << 32;
/// `round(2^32 * 2^(-2^-m))` for `m` = 1 to 6: the factors of a weight's fraction of a
/// half-life, one per bit of the 6-bit fraction, the highest bit first.
const FRACTION: [u128; 6] = [
    3_037_000_500,
    3_611_622_603,
    3_938_502_376,
    4_112_874_773,
    4_202_935_003,
    4_248_701_965,
];

/// How a 2 s slot reduces its seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    /// The mean of the seconds, rounded half up (CPU).
    Mean,
    /// The largest second (memory).
    Max,
}

/// What a profile measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    /// How a slot reduces its seconds.
    pub combine: Combine,
    /// The value of the histogram's highest bucket; larger values count in it too.
    pub top: u32,
}

/// CPU in sixteenths of a core; the histogram tops at 88 cores.
pub const CPU_UNIT: Unit = Unit {
    combine: Combine::Mean,
    top: 1408,
};
/// Memory in MiB; the histogram tops at 1 TiB.
pub const MEMORY_UNIT: Unit = Unit {
    combine: Combine::Max,
    top: 1 << 20,
};

/// A run's series as the learner reads it: points in the unit, `granularity_secs`
/// seconds a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSeries<'a> {
    /// Seconds one point covers; 0 reads as 1.
    pub granularity_secs: u32,
    /// The points, oldest first.
    pub points: &'a [u32],
    /// The run's wall time in microseconds; it ends the last point.
    pub run_usec: u64,
}

/// What a check found and what was learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    /// Whether the run was added to the profile.
    pub learned: bool,
    /// Kept runs after the call.
    pub runs: usize,
    /// The run's mean (its total over its seconds), rounded half up.
    pub actual_mean: u32,
    /// The run's largest 2 s slot value (start) or point (tail).
    pub actual_peak: u32,
    /// The level before learning; `None` for an empty profile.
    pub predicted_level: Option<u32>,
    /// The largest `slot_p90` over the start slots the run reached, before learning;
    /// `None` when the run was not scored.
    pub predicted_peak: Option<u32>,
    /// The curve's error in unit-seconds; `None` when not scored (fewer than
    /// [`MIN_RUNS`] kept runs before the run).
    pub curve_error: Option<u64>,
    /// The flat level's error in unit-seconds; `None` when not scored.
    pub flat_error: Option<u64>,
    /// The self-check window after the call.
    pub window: Window,
    /// Whether the profile is shaped after the call.
    pub shaped: bool,
}

/// The sum of the last scored runs' errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Window {
    /// Scored runs in the window, at most [`CHECK_RUNS`].
    pub runs: usize,
    /// The curve's error over them, in unit-seconds.
    pub curve_error: u64,
    /// The flat level's error over them, in unit-seconds.
    pub flat_error: u64,
}

impl Window {
    /// The curve's error per mille of flat's; `None` when flat's error is 0.
    pub fn fit_permille(self) -> Option<u64> {
        (u128::from(self.curve_error) * 1000)
            .checked_div(u128::from(self.flat_error))
            .map(|q| u64::try_from(q).unwrap_or(u64::MAX))
    }
}

/// A kept run: what the profile needs of it, not the series.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    /// When the run ended, in ms since the epoch.
    ended_ms: u64,
    /// The value of each start slot the run reached.
    slots: Vec<u32>,
    /// Seconds of the last slot the run covered (1 or 2); the other slots have 2.
    covered_last: u8,
    /// The sum of the per-second values of the seconds before 128 s.
    start_sum: u64,
    /// The sum of the per-second values from 128 s on.
    tail_sum: u64,
    /// Seconds from 128 s on.
    tail_secs: u64,
}

impl Kept {
    /// Seconds slot `s` holds of this run.
    fn cover(&self, s: usize) -> u64 {
        if s + 1 == self.slots.len() {
            u64::from(self.covered_last)
        } else {
            SLOT
        }
    }

    /// The run's seconds.
    fn secs(&self) -> u64 {
        let full = self.slots.len().saturating_sub(1) as u64;
        full * SLOT + u64::from(self.covered_last) + self.tail_secs
    }

    /// The sum of the run's per-second values.
    fn total(&self) -> u128 {
        u128::from(self.start_sum) + u128::from(self.tail_sum)
    }
}

/// A run read for checking: the record to keep and what the check needs besides.
struct Parsed {
    kept: Kept,
    /// Each tail span as its seconds in the tail and its value.
    tail_spans: Vec<(u64, u32)>,
    peak: u32,
    secs: u64,
}

/// A class's profile in one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    unit: Unit,
    runs: VecDeque<Kept>,
    scores: VecDeque<(u64, u64)>,
}

impl Profile {
    /// An empty profile.
    pub fn new(unit: Unit) -> Self {
        Self {
            unit,
            runs: VecDeque::new(),
            scores: VecDeque::new(),
        }
    }

    /// Checks `run`, ended at `ended_ms` (ms since the epoch), against the profile as it
    /// stands, then learns it when `clean`, or when not clean and the run's mean is above
    /// the current level (a run slowed by contention or held at a ceiling may only raise
    /// its class). A series with no point teaches and scores nothing.
    pub fn check_and_learn(&mut self, run: RunSeries<'_>, ended_ms: u64, clean: bool) -> Checked {
        let mut out = Checked {
            learned: false,
            runs: self.runs.len(),
            actual_mean: 0,
            actual_peak: 0,
            predicted_level: None,
            predicted_peak: None,
            curve_error: None,
            flat_error: None,
            window: self.window(),
            shaped: self.shaped(),
        };
        let Some(parsed) = parse(self.unit, run, ended_ms) else {
            return out;
        };
        let weights = self.weights(ended_ms);
        let level = self.level_with(&weights);
        out.predicted_level = level;
        out.actual_peak = parsed.peak;
        out.actual_mean = ratio(parsed.kept.total(), u128::from(parsed.secs)).unwrap_or(0);
        let scored = self.runs.len() >= MIN_RUNS;
        if let Some((curve, flat, peak)) = scored.then(|| self.score(&parsed, &weights, level)) {
            self.scores.push_back((curve, flat));
            if self.scores.len() > CHECK_RUNS {
                self.scores.pop_front();
            }
            out.curve_error = Some(curve);
            out.flat_error = Some(flat);
            out.predicted_peak = peak;
        }
        let raises = level.is_some_and(|l| out.actual_mean > l);
        out.learned = clean | raises;
        if out.learned {
            self.runs.push_back(parsed.kept);
            if self.runs.len() > MAX_RUNS {
                self.runs.pop_front();
            }
        }
        out.runs = self.runs.len();
        out.window = self.window();
        out.shaped = self.shaped();
        out
    }

    /// Kept runs.
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// The decayed mean of the per-second samples at `at_ms`; `None` with no run.
    pub fn level(&self, at_ms: u64) -> Option<u32> {
        self.level_with(&self.weights(at_ms))
    }

    /// The decayed mean of start slot `slot` at `at_ms`; `None` when no run reached it
    /// or `slot` is not a start slot.
    pub fn slot_mean(&self, slot: usize, at_ms: u64) -> Option<u32> {
        self.mean_with(&self.weights(at_ms), slot)
    }

    /// The load-weighted 90th percentile of start slot `slot` at `at_ms`: the first value,
    /// from the lowest, at which the runs' loads (weight x seconds x value) reach 90% of
    /// all. `Some(0)` when runs reached the slot but their total load is 0 (all values 0,
    /// or every run too old to weigh); `None` when no run reached it.
    pub fn slot_p90(&self, slot: usize, at_ms: u64) -> Option<u32> {
        self.p90_with(&self.weights(at_ms), slot)
    }

    /// Kept runs that reached start slot `slot`.
    pub fn slot_runs(&self, slot: usize) -> usize {
        self.runs.iter().filter(|r| r.slots.len() > slot).count()
    }

    /// The decayed mean after 128 s at `at_ms`; `None` when no run lasted past 128 s.
    pub fn tail(&self, at_ms: u64) -> Option<u32> {
        self.tail_with(&self.weights(at_ms))
    }

    /// The decayed seconds of every kept run's samples in each log-spaced bucket at
    /// `at_ms` (weight in Q32 x seconds). Each run adds, per start slot, the slot's
    /// seconds at its value, and its tail seconds at the tail's mean.
    pub fn histogram(&self, at_ms: u64) -> [u64; BUCKETS] {
        let weights = self.weights(at_ms);
        let edges = edges(self.unit);
        let mut hist = [0u64; BUCKETS];
        for (run, &w) in self.runs.iter().zip(&weights) {
            for (s, &v) in run.slots.iter().enumerate() {
                let b = bucket(&edges, v);
                hist[b] = hist[b].saturating_add(w.saturating_mul(run.cover(s)));
            }
            if let Some(v) = ratio(u128::from(run.tail_sum), u128::from(run.tail_secs)) {
                let b = bucket(&edges, v);
                hist[b] = hist[b].saturating_add(w.saturating_mul(run.tail_secs));
            }
        }
        hist
    }

    /// The load-weighted 90th percentile of all samples at `at_ms`, read from the
    /// histogram: the upper edge of the first bucket at which the load (seconds x the
    /// edge) reaches 90% of all. `None` when the load is 0.
    pub fn level_p90(&self, at_ms: u64) -> Option<u32> {
        let items: Vec<(u32, u128)> = self
            .histogram(at_ms)
            .iter()
            .zip(edges(self.unit))
            .map(|(&n, edge)| (edge, u128::from(n) * u128::from(edge)))
            .collect();
        p90(&items)
    }

    /// The sum of the last [`CHECK_RUNS`] scored runs' errors.
    pub fn window(&self) -> Window {
        let sum = |pick: fn(&(u64, u64)) -> u64| {
            self.scores
                .iter()
                .fold(0u64, |a, s| a.saturating_add(pick(s)))
        };
        Window {
            runs: self.scores.len(),
            curve_error: sum(|s| s.0),
            flat_error: sum(|s| s.1),
        }
    }

    /// Whether the class has a shape: it keeps [`MIN_RUNS`] runs, the window is full and
    /// the curve's error is at most [`FIT_PERMILLE`] per mille of flat's.
    pub fn shaped(&self) -> bool {
        shaped_by(self.runs.len(), self.window())
    }

    /// The weight of each kept run at `at_ms`, in Q32.
    fn weights(&self, at_ms: u64) -> Vec<u64> {
        self.runs
            .iter()
            .map(|r| weight(at_ms.saturating_sub(r.ended_ms)))
            .collect()
    }

    fn level_with(&self, weights: &[u64]) -> Option<u32> {
        let (num, den) = self
            .runs
            .iter()
            .zip(weights)
            .fold((0u128, 0u128), |a, (r, &w)| {
                let w = u128::from(w);
                (a.0 + w * r.total(), a.1 + w * u128::from(r.secs()))
            });
        ratio(num, den)
    }

    fn tail_with(&self, weights: &[u64]) -> Option<u32> {
        let (num, den) = self
            .runs
            .iter()
            .zip(weights)
            .fold((0u128, 0u128), |a, (r, &w)| {
                let w = u128::from(w);
                (
                    a.0 + w * u128::from(r.tail_sum),
                    a.1 + w * u128::from(r.tail_secs),
                )
            });
        ratio(num, den)
    }

    /// The runs that reached `slot` as value, weight and seconds.
    fn samples(&self, weights: &[u64], slot: usize) -> Vec<(u32, u128, u128)> {
        self.runs
            .iter()
            .zip(weights)
            .filter_map(|(r, &w)| {
                r.slots
                    .get(slot)
                    .map(|&v| (v, u128::from(w), u128::from(r.cover(slot))))
            })
            .collect()
    }

    fn mean_with(&self, weights: &[u64], slot: usize) -> Option<u32> {
        let (num, den) = self
            .samples(weights, slot)
            .iter()
            .fold((0u128, 0u128), |a, &(v, w, c)| {
                (a.0 + w * c * u128::from(v), a.1 + w * c)
            });
        ratio(num, den)
    }

    fn p90_with(&self, weights: &[u64], slot: usize) -> Option<u32> {
        let mut samples = self.samples(weights, slot);
        samples.sort_by_key(|s| s.0);
        let items: Vec<(u32, u128)> = samples
            .iter()
            .map(|&(v, w, c)| (v, w * c * u128::from(v)))
            .collect();
        p90(&items).or(items.first().map(|_| 0))
    }

    /// The curve's and the flat level's errors for `run`, and its predicted peak, against
    /// the profile as it stands.
    fn score(&self, run: &Parsed, weights: &[u64], level: Option<u32>) -> (u64, u64, Option<u32>) {
        let flat = level.unwrap_or(0);
        let tail = self.tail_with(weights).unwrap_or(flat);
        let starts = run.kept.slots.iter().enumerate().map(|(s, &v)| {
            let c = run.kept.cover(s);
            let curve = self
                .mean_with(weights, s)
                .filter(|_| self.slot_runs(s) >= MIN_SLOT_RUNS)
                .unwrap_or(flat);
            (
                c * u64::from(v.abs_diff(curve)),
                c * u64::from(v.abs_diff(flat)),
            )
        });
        let tails = run.tail_spans.iter().map(|&(secs, v)| {
            (
                secs.saturating_mul(u64::from(v.abs_diff(tail))),
                secs.saturating_mul(u64::from(v.abs_diff(flat))),
            )
        });
        let (curve, flat_err) = starts.chain(tails).fold((0u64, 0u64), |a, e| {
            (a.0.saturating_add(e.0), a.1.saturating_add(e.1))
        });
        let peak = (0..run.kept.slots.len())
            .filter_map(|s| self.p90_with(weights, s))
            .max();
        (curve, flat_err, peak)
    }
}

/// Whether a profile with `runs` kept runs and this window is shaped.
pub(crate) fn shaped_by(runs: usize, window: Window) -> bool {
    [
        runs >= MIN_RUNS,
        window.runs == CHECK_RUNS,
        window.flat_error > 0,
        u128::from(window.curve_error) * 1000
            <= u128::from(FIT_PERMILLE) * u128::from(window.flat_error),
    ]
    .into_iter()
    .all(|b| b)
}

/// `num / den` rounded half up; `None` when `den` is 0.
fn ratio(num: u128, den: u128) -> Option<u32> {
    (num * 2 + den)
        .checked_div(den * 2)
        .map(|q| u32::try_from(q).unwrap_or(u32::MAX))
}

/// The first item, in the order given, at which the cumulative load reaches 90% of the
/// total; `None` when the total is 0.
fn p90<T: Copy>(items: &[(T, u128)]) -> Option<T> {
    let total: u128 = items.iter().map(|i| i.1).sum();
    if total == 0 {
        return None;
    }
    let mut cum = 0u128;
    items
        .iter()
        .find(|i| {
            cum += i.1;
            cum * 10 >= total * 9
        })
        .map(|i| i.0)
}

/// The weight of a run `age_ms` old in Q32: `2^(-age / HALF_LIFE_MS)`. Whole half-lives
/// shift; the fraction (in 64ths) multiplies the factor of each of its set bits.
pub(crate) fn weight(age_ms: u64) -> u64 {
    let whole = u32::try_from(age_ms / HALF_LIFE_MS).unwrap_or(u32::MAX);
    let base = ONE.checked_shr(whole).unwrap_or(0);
    let fraction = (age_ms % HALF_LIFE_MS) * 64 / HALF_LIFE_MS;
    let one = u128::from(ONE);
    let w = FRACTION
        .iter()
        .enumerate()
        .fold(u128::from(base), |w, (i, &f)| {
            let bit = u128::from((fraction >> (5 - i)) & 1);
            (w * (one - bit * (one - f))) >> 32
        });
    u64::try_from(w).unwrap_or(u64::MAX)
}

/// The histogram's upper edges: edge 0 is 1, the last is the unit's top, and between
/// them each is the ceiling of a Q32 power of the ratio whose 63rd power is the top.
/// Edges never fall, and several low ones may be equal.
pub(crate) fn edges(unit: Unit) -> [u32; BUCKETS] {
    let top = unit.top.max(1);
    let target = u128::from(top) << 32;
    let power = |r: u128, n: usize| (0..n).fold(u128::from(ONE), |p, _| p.saturating_mul(r) >> 32);
    let (mut lo, mut hi) = (u128::from(ONE), 2 * u128::from(ONE));
    for _ in 0..40 {
        let mid = (lo + hi) / 2;
        if power(mid, BUCKETS - 1) <= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mut out = [top; BUCKETS];
    out[0] = 1;
    for b in 1..BUCKETS - 1 {
        let ceil = (power(lo, b) + u128::from(ONE) - 1) >> 32;
        out[b] = u32::try_from(ceil).unwrap_or(top).min(top).max(out[b - 1]);
    }
    out
}

/// The bucket a value counts in: the first whose edge reaches it, the last for a value
/// above the top.
fn bucket(edges: &[u32; BUCKETS], value: u32) -> usize {
    edges.partition_point(|&e| e < value).min(BUCKETS - 1)
}

/// A run's spans as start second, seconds and value, one per point. A point covers
/// `granularity_secs` seconds (at least 1); the last covers up to the run's wall time
/// rounded up to a whole second, but at least one second and no more than a point.
pub(crate) fn spans(run: RunSeries<'_>) -> Vec<(u64, u64, u32)> {
    let g = u64::from(run.granularity_secs.max(1));
    let n = run.points.len() as u64;
    run.points
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let i = i as u64;
            let start = i * g;
            let len = if i + 1 == n {
                run.run_usec.div_ceil(1_000_000).clamp(start + 1, n * g) - start
            } else {
                g
            };
            (start, len, v)
        })
        .collect()
}

/// Reads a run into the record to keep; `None` for a series with no point.
fn parse(unit: Unit, run: RunSeries<'_>, ended_ms: u64) -> Option<Parsed> {
    let spans = spans(run);
    let secs = spans.last().map(|&(start, len, _)| start + len)?;
    let mut sum = [0u64; START_SLOTS];
    let mut cnt = [0u64; START_SLOTS];
    let mut top = [0u32; START_SLOTS];
    let (mut tail_sum, mut tail_secs, mut tail_peak) = (0u64, 0u64, 0u32);
    let mut tail_spans = Vec::new();
    for &(start, len, v) in &spans {
        let end = start + len;
        let last = ((end - 1) / SLOT).min(START_SLOTS as u64 - 1);
        for s in (start / SLOT)..=last {
            let n = end.min(SLOT * s + SLOT) - start.max(SLOT * s);
            let i = s as usize;
            sum[i] += n * u64::from(v);
            cnt[i] += n;
            top[i] = top[i].max(v);
        }
        let in_tail = end.saturating_sub(start.max(START_SECS));
        if in_tail > 0 {
            tail_sum += in_tail * u64::from(v);
            tail_secs += in_tail;
            tail_peak = tail_peak.max(v);
            tail_spans.push((in_tail, v));
        }
    }
    let reached = secs.div_ceil(SLOT).min(START_SLOTS as u64) as usize;
    let slots: Vec<u32> = (0..reached)
        .map(|s| match unit.combine {
            Combine::Mean => u32::try_from((sum[s] + cnt[s] / 2) / cnt[s]).unwrap_or(u32::MAX),
            Combine::Max => top[s],
        })
        .collect();
    let peak = slots.iter().copied().fold(tail_peak, u32::max);
    Some(Parsed {
        kept: Kept {
            ended_ms,
            covered_last: cnt[reached - 1] as u8,
            start_sum: sum.iter().sum(),
            tail_sum,
            tail_secs,
            slots,
        },
        tail_spans,
        peak,
        secs,
    })
}

/// Whether a CPU series agrees with the run's exact CPU time: its total (sum of point x
/// span seconds, in sixteenths of a core-second) differs from `cpu_usage_usec` by at most
/// 5% of `cpu_usage_usec` (a zero usage agrees only with a zero total).
pub fn series_agrees(run: RunSeries<'_>, cpu_usage_usec: u64) -> bool {
    let total: u128 = spans(run)
        .iter()
        .map(|&(_, len, v)| u128::from(len) * u128::from(v))
        .sum();
    let exact = u128::from(cpu_usage_usec) * 16;
    (total * 1_000_000).abs_diff(exact) * 20 <= exact
}
