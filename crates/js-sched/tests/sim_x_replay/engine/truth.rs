//! What really happens to a run: its CPU demand tick by tick and its working set, drawn
//! from a fixture row and keyed random values. The scheduler's beliefs live elsewhere and
//! never read this module.
//!
//! # Keyed draws
//!
//! There is no shared random stream. Every random quantity is a pure function
//! `mix(seed, domain, subject, index)` ([`mix`]) of the world's seed, a `DOMAIN_*` constant
//! naming what is drawn, the thing it is drawn for (a lease, a key) and an index within
//! it. Adding a lease, a ranker or a retry therefore shifts no other draw, and a run is
//! built from its own row, its own position `lease_ix` and the configuration alone.
//!
//! # The run
//!
//! * Time is in 1 s ticks. A run occupies `ceil(wall / 1 s)` ticks (at least one) and its
//!   ticks sum to exactly `cpu_ms` core-milliseconds. Minute `m` is ticks `60 m..60 m + 60`
//!   (the last one may be short) and windows are runs of 5 ticks from the run's start.
//! * The minute series: a run of at least [`SHAPE_MIN_WALL_MS`] whose key has a shaped class
//!   spreads `cpu_ms` over its minutes in proportion to the class template summed over the
//!   minute's ticks, moved by a shift of -1, 0 or +1 minute and multiplied by noise from
//!   [`tables::LN15`]. Every other run, including every run of a flat key, spreads it in
//!   proportion to the ticks in each minute. [`Fidelity::Identical`] draws no shift and
//!   no noise. Shares are cut by [`apportion`], so the minute totals sum to `cpu_ms`
//!   exactly.
//! * Bursts: inside a minute each window gets a weight from [`tables::LN32`] times its
//!   ticks, and the minute's core-ms are cut by [`apportion`] over the windows; a window's
//!   core-ms are cut equally over its ticks. A minute's ticks therefore sum to its series
//!   value exactly, whatever the bursts were. Bursts shape the 5 s windows a host sees;
//!   nothing observes a finer split than the minute series.
//! * The working set ramps linearly to the row's peak by tick 180 (or the run's last tick
//!   if it is shorter). In a scenario that scales memory, a key's runs that start in one
//!   60 s span share a draw that multiplies the peak with probability 1 in
//!   [`MEM_SPIKE_ONE_IN`].
//!
//! Rows that did not end `ok` replay as recorded; this module only builds their truth, and
//! keeping them out of a learner is the learner's rule.

use super::fixture::{KeyIx, Outcome, Row};
use super::tables::{self, ONE_PPM};

/// The SplitMix64 finaliser applied to `x + gamma`: the output of the generator whose
/// state before the step is `x`. A bijection of `u64`.
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The keyed draw: 64 uniform bits for (`seed`, `domain`, `subject`, `index`).
///
/// `subject` is the lease, key or digest the quantity belongs to and `index` counts the
/// draws of that quantity (a minute, a window); pass 0 where there is only one. Each
/// argument goes through a full [`splitmix64`] step, so changing any one of them, or
/// swapping two, gives an unrelated value.
pub fn mix(seed: u64, domain: u64, subject: u64, index: u64) -> u64 {
    let h = splitmix64(seed);
    let h = splitmix64(h ^ domain);
    let h = splitmix64(h ^ subject);
    splitmix64(h ^ index)
}

/// Domain of the shape class of a key (subject: key index).
pub const DOMAIN_CLASS: u64 = 1;
/// Domain of a run's shift of its template (subject: lease).
pub const DOMAIN_SHIFT: u64 = 2;
/// Domain of a run's per-minute shape noise (subject: lease, index: minute).
pub const DOMAIN_NOISE: u64 = 3;
/// Domain of a window's burst (subject: lease, index: window of the run).
pub const DOMAIN_BURST: u64 = 4;
/// Domain of the memory multiplier of a key in a 60 s span (subject: key, index: span).
pub const DOMAIN_MEM_SPIKE: u64 = 5;
/// Domain of a hand-made world's run mean (subject: lease).
pub const DOMAIN_RUN_MEAN: u64 = 6;
/// Domain of the random ranker's choice (subject: lease; the ranker picks the index).
pub const DOMAIN_RANDOM: u64 = 7;
/// Domain of the control's permutation of a key's belief minutes (subject: key digest).
pub const DOMAIN_PERM: u64 = 8;

/// Every domain, to check they are distinct.
pub const DOMAINS: [u64; 8] = [
    DOMAIN_CLASS,
    DOMAIN_SHIFT,
    DOMAIN_NOISE,
    DOMAIN_BURST,
    DOMAIN_MEM_SPIKE,
    DOMAIN_RUN_MEAN,
    DOMAIN_RANDOM,
    DOMAIN_PERM,
];

/// Ticks per minute.
pub const MINUTE_TICKS: u64 = 60;

/// Ticks per window.
pub const WINDOW_TICKS: u64 = 5;

/// A run shorter than this (180 s) is flat whatever its key's class.
pub const SHAPE_MIN_WALL_MS: u64 = 180_000;

/// The working set reaches its peak at this tick (minute 3) or at the run's end.
pub const MEM_RAMP_TICKS: u64 = 180;

/// A key's memory multiplier applies with probability one in this many.
pub const MEM_SPIKE_ONE_IN: u64 = 10;

/// The shape a key's runs follow over their life, as a template of relative CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    /// The same demand at every tick.
    Flat,
    /// Idle for the first sixth of the run, then constant.
    IdlePrelude,
    /// A quarter of the later level for the first 0.7 of the run, then the later level.
    LatePeak,
    /// Demand rising linearly from nothing to twice its mean.
    Ramp,
    /// Idle for the first two fifths of the run, then constant.
    Sleeper,
}

impl Shape {
    /// Every shape, in the order of a [`ShapeMix`].
    pub const ALL: [Shape; 5] = [
        Shape::Flat,
        Shape::IdlePrelude,
        Shape::LatePeak,
        Shape::Ramp,
        Shape::Sleeper,
    ];

    /// The template at tick `t` of a run of `ticks` ticks, in ppm of the run's mean.
    ///
    /// The template is read at the middle of the tick, `u = (2 t + 1) / (2 ticks)`, so
    /// summed over all ticks its mean is 1 (the ramp and the steps are exact up to
    /// rounding). The levels are the shape's relative heights divided by its mean:
    /// idle prelude 6/5, sleeper 5/3, late peak 0.5/0.95 and 2/0.95.
    ///
    /// # Panics
    ///
    /// If `ticks` is zero.
    pub fn weight_ppm(self, t: u64, ticks: u64) -> u64 {
        assert!(ticks > 0, "a run has at least one tick");
        let num = 2 * t + 1;
        let den = 2 * ticks;
        match self {
            Shape::Flat => ONE_PPM,
            Shape::IdlePrelude => {
                if 6 * num < den {
                    0
                } else {
                    1_200_000
                }
            }
            Shape::Sleeper => {
                if 5 * num < 2 * den {
                    0
                } else {
                    1_666_667
                }
            }
            Shape::LatePeak => {
                if 10 * num < 7 * den {
                    526_316
                } else {
                    2_105_263
                }
            }
            Shape::Ramp => ONE_PPM * num / ticks,
        }
    }
}

/// Parts per thousand of the keys of a world in each [`Shape`], in `Shape::ALL` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeMix {
    permille: [u16; 5],
}

impl ShapeMix {
    /// The default world: flat 40%, idle prelude 20%, late peak 15%, ramp 15%, sleeper 10%.
    pub const DEFAULT: ShapeMix = ShapeMix {
        permille: [400, 200, 150, 150, 100],
    };

    /// The null world: every key is flat.
    pub const NULL: ShapeMix = ShapeMix {
        permille: [1000, 0, 0, 0, 0],
    };

    /// A mix with the given parts per thousand in `Shape::ALL` order.
    ///
    /// # Panics
    ///
    /// If they do not sum to 1000.
    pub fn new(permille: [u16; 5]) -> ShapeMix {
        let sum: u32 = permille.iter().map(|&p| u32::from(p)).sum();
        assert_eq!(sum, 1000, "a shape mix sums to 1000");
        ShapeMix { permille }
    }

    /// The class of `key` in the world seeded `seed`: a draw from `mix(seed, DOMAIN_CLASS,
    /// key)`, so the seed changes the assignment and every run of a key shares its class.
    pub fn class_of(&self, seed: u64, key: KeyIx) -> Shape {
        let mut u = mix(seed, DOMAIN_CLASS, u64::from(key.0), 0) % 1000;
        for (shape, &p) in Shape::ALL.iter().zip(&self.permille) {
            if u < u64::from(p) {
                return *shape;
            }
            u -= u64::from(p);
        }
        // Unreachable: the parts sum to 1000 and `u` is below it.
        Shape::Flat
    }
}

/// How faithful a run's shape is to its key's template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fidelity {
    /// No shift and no noise: every run of a key follows the same template. An upper bound
    /// on what a learner can know.
    Identical,
    /// The default: a shift of -1, 0 or +1 minute and per-minute noise from `LN15`.
    Jittered,
}

/// Everything that makes one truth world, other than the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TruthConfig {
    /// The world's seed: it keys every draw.
    pub seed: u64,
    /// Shape fidelity.
    pub fidelity: Fidelity,
    /// Shape classes of keys.
    pub mix: ShapeMix,
    /// The multiplier of the memory scenario in parts per thousand (1300 for 1.3), or 1000
    /// for none.
    pub mem_scale_permille: u64,
}

impl TruthConfig {
    /// The default world for `seed`: jittered shapes, the default mix, no memory scenario.
    pub fn new(seed: u64) -> TruthConfig {
        TruthConfig {
            seed,
            fidelity: Fidelity::Jittered,
            mix: ShapeMix::DEFAULT,
            mem_scale_permille: 1000,
        }
    }
}

/// `total` cut into `weights.len()` integer parts proportional to the weights, summing to
/// `total` exactly: each part is its floor share and the leftover units go one each to the
/// largest fractional remainders, the lower index winning a tie. All-zero weights cut
/// `total` equally. Each weight must be below 2^64 (the product with `total` is held in a
/// `u128`).
pub fn apportion(total: u64, weights: &[u128]) -> Vec<u64> {
    if weights.is_empty() {
        return Vec::new();
    }
    let sum: u128 = weights.iter().sum();
    let equal = vec![1u128; weights.len()];
    let (weights, sum) = if sum == 0 {
        (equal.as_slice(), weights.len() as u128)
    } else {
        (weights, sum)
    };
    let total_wide = u128::from(total);
    // Floor shares and remainders: `total * w = share * sum + rem`, share below 2^64.
    let mut parts: Vec<u64> = Vec::with_capacity(weights.len());
    let mut rems: Vec<(u128, usize)> = Vec::with_capacity(weights.len());
    for (i, &w) in weights.iter().enumerate() {
        let product = total_wide * w;
        parts.push((product / sum) as u64);
        rems.push((product % sum, i));
    }
    let given: u64 = parts.iter().sum();
    let leftover = (total - given) as usize;
    rems.sort_by_key(|&(rem, i)| (std::cmp::Reverse(rem), i));
    for &(_, i) in rems.iter().take(leftover) {
        parts[i] += 1;
    }
    parts
}

/// A hand-made world's run: `key_mean_cpu_ms` scaled by a draw from [`tables::LN2245`], so
/// the runs of a key vary around its mean as a lognormal does (median over P90 of 0.75). A trace's runs carry their own means and do not use this.
pub fn hand_run_cpu_ms(seed: u64, lease_ix: u32, key_mean_cpu_ms: u64) -> u64 {
    let factor = tables::draw(
        &tables::LN2245,
        mix(seed, DOMAIN_RUN_MEAN, u64::from(lease_ix), 0),
    );
    (u128::from(key_mean_cpu_ms) * u128::from(factor) / u128::from(ONE_PPM)) as u64
}

/// The truth of one run: what it demands and holds, minute by minute and tick by tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The run's position in the sorted rows; it names every draw made for the run.
    pub lease_ix: u32,
    /// Its work key.
    pub key: KeyIx,
    /// The class of its key in this world.
    pub shape: Shape,
    /// How the recorded action ended.
    pub outcome: Outcome,
    /// When it arrives, ms from the trace start.
    pub start_ms: u64,
    /// Wall time, ms.
    pub wall_ms: u64,
    /// CPU time over the run, ms.
    pub cpu_ms: u64,
    /// Peak working set, MiB, after the memory scenario.
    pub peak_mem_mib: u64,
    seed: u64,
    minute_core_ms: Vec<u64>,
}

impl Run {
    /// The truth of `row`, the `lease_ix`-th of its fixture, in the world `cfg`.
    ///
    /// A function of its three arguments only, so a world is the same however its runs are
    /// built or ordered.
    pub fn new(row: &Row, lease_ix: u32, cfg: &TruthConfig) -> Run {
        let shape = cfg.mix.class_of(cfg.seed, row.key);
        let mut run = Run {
            lease_ix,
            key: row.key,
            shape,
            outcome: row.outcome,
            start_ms: row.start_ms,
            wall_ms: row.wall_ms,
            cpu_ms: row.cpu_ms,
            peak_mem_mib: scaled_peak(row, cfg),
            seed: cfg.seed,
            minute_core_ms: Vec::new(),
        };
        let weights = run.minute_weights(cfg);
        run.minute_core_ms = apportion(row.cpu_ms, &weights);
        run
    }

    /// Ticks the run occupies.
    pub fn ticks(&self) -> u64 {
        self.wall_ms.div_ceil(1000).max(1)
    }

    /// Minutes the run touches.
    pub fn minutes(&self) -> usize {
        self.ticks().div_ceil(MINUTE_TICKS) as usize
    }

    /// Ticks in minute `m`: 60, or fewer for the last minute.
    ///
    /// # Panics
    ///
    /// If `m` is past the run.
    pub fn ticks_in_minute(&self, m: usize) -> u64 {
        assert!(m < self.minutes(), "minute {m} is past the run");
        (self.ticks() - m as u64 * MINUTE_TICKS).min(MINUTE_TICKS)
    }

    /// Core-ms the run uses in minute `m`: the minute series. They sum to `cpu_ms`.
    pub fn minute_core_ms(&self, m: usize) -> u64 {
        self.minute_core_ms[m]
    }

    /// The mean cores of minute `m` in millicores, floored: core-ms over the minute's
    /// ticks.
    pub fn minute_millicores(&self, m: usize) -> u64 {
        self.minute_core_ms[m] / self.ticks_in_minute(m)
    }

    /// The mean cores of the whole recorded action in millicores, floored: `cpu_ms / wall_ms`
    /// with the wall as recorded.
    pub fn mean_millicores(&self) -> u64 {
        (u128::from(self.cpu_ms) * 1000 / u128::from(self.wall_ms.max(1))) as u64
    }

    /// The working set in MiB after tick `t`, when it has ramped for `t + 1` ticks.
    pub fn working_set_mib(&self, t: u64) -> u64 {
        let ramp = MEM_RAMP_TICKS.min(self.ticks());
        (u128::from(self.peak_mem_mib) * u128::from((t + 1).min(ramp)) / u128::from(ramp)) as u64
    }

    /// The working set at the end of minute `m`, which is its largest in the minute.
    pub fn minute_working_set_mib(&self, m: usize) -> u64 {
        self.working_set_mib(m as u64 * MINUTE_TICKS + self.ticks_in_minute(m) - 1)
    }

    /// Core-ms of each window of minute `m`: the bursts. They sum to the minute's value.
    pub fn minute_windows(&self, m: usize) -> Vec<u64> {
        let n = self.ticks_in_minute(m);
        let first_window = m as u64 * (MINUTE_TICKS / WINDOW_TICKS);
        let weights: Vec<u128> = (0..n.div_ceil(WINDOW_TICKS))
            .map(|j| {
                let ticks = (n - j * WINDOW_TICKS).min(WINDOW_TICKS);
                let burst = tables::draw(
                    &tables::LN32,
                    mix(
                        self.seed,
                        DOMAIN_BURST,
                        u64::from(self.lease_ix),
                        first_window + j,
                    ),
                );
                u128::from(burst) * u128::from(ticks)
            })
            .collect();
        apportion(self.minute_core_ms[m], &weights)
    }

    /// Core-ms of each tick of minute `m`; they sum to the minute's value.
    pub fn minute_ticks(&self, m: usize) -> Vec<u64> {
        let n = self.ticks_in_minute(m);
        let mut out = Vec::new();
        for (j, window) in self.minute_windows(m).into_iter().enumerate() {
            let ticks = (n - j as u64 * WINDOW_TICKS).min(WINDOW_TICKS);
            // Equal split; the first `window % ticks` ticks carry the odd core-ms.
            out.extend((0..ticks).map(|i| window / ticks + u64::from(i < window % ticks)));
        }
        out
    }

    /// Core-ms of every tick of the run; they sum to `cpu_ms`.
    pub fn all_ticks(&self) -> Vec<u64> {
        (0..self.minutes())
            .flat_map(|m| self.minute_ticks(m))
            .collect()
    }

    /// The relative weight of each minute, before the cut of `cpu_ms`.
    fn minute_weights(&self, cfg: &TruthConfig) -> Vec<u128> {
        let ticks = self.ticks();
        let shaped = self.wall_ms >= SHAPE_MIN_WALL_MS && self.shape != Shape::Flat;
        let (shift, jittered) = match cfg.fidelity {
            Fidelity::Jittered if shaped => {
                let s = mix(self.seed, DOMAIN_SHIFT, u64::from(self.lease_ix), 0) % 3;
                (s as i64 - 1, true)
            }
            _ => (0, false),
        };
        (0..self.minutes())
            .map(|m| {
                let n = self.ticks_in_minute(m);
                if !shaped {
                    return u128::from(n);
                }
                let from = m as u64 * MINUTE_TICKS;
                let level: u128 = (from..from + n)
                    .map(|t| u128::from(self.shape.weight_ppm(shifted(t, shift, ticks), ticks)))
                    .sum();
                if !jittered {
                    return level;
                }
                let noise = tables::draw(
                    &tables::LN15,
                    mix(self.seed, DOMAIN_NOISE, u64::from(self.lease_ix), m as u64),
                );
                level * u128::from(noise)
            })
            .collect()
    }
}

/// Tick `t` read through a template delayed by `shift` minutes (negative: advanced), the
/// ends held.
pub fn shifted(t: u64, shift: i64, ticks: u64) -> u64 {
    let by = shift.unsigned_abs() * MINUTE_TICKS;
    if shift >= 0 {
        t.saturating_sub(by)
    } else {
        (t + by).min(ticks - 1)
    }
}

/// The row's peak after the memory scenario: scaled when the key's draw for the 60 s span
/// of the row's start comes up.
fn scaled_peak(row: &Row, cfg: &TruthConfig) -> u64 {
    if cfg.mem_scale_permille == 1000 {
        return row.peak_mem_mib;
    }
    let draw = mix(
        cfg.seed,
        DOMAIN_MEM_SPIKE,
        u64::from(row.key.0),
        row.start_ms / 60_000,
    );
    if draw.is_multiple_of(MEM_SPIKE_ONE_IN) {
        (u128::from(row.peak_mem_mib) * u128::from(cfg.mem_scale_permille) / 1000) as u64
    } else {
        row.peak_mem_mib
    }
}

/// The truth of every row, `lease_ix` being its position: the world for `cfg`.
pub fn build(rows: &[Row], cfg: &TruthConfig) -> Vec<Run> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| Run::new(row, u32::try_from(i).expect("fewer than 2^32 rows"), cfg))
        .collect()
}
