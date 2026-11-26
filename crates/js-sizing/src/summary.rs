//! The per-key summary: what is kept, how a run enters it, what it estimates.

use crate::key::{Basis, Day};
use crate::run::{Bounds, CpuSample, Discard, Observed, Outcome, Run};
use crate::{GIB, MIB};

/// The smallest memory estimate.
pub const MIN_MEMORY: u64 = GIB;
/// Samples before the estimate may go below the default booking.
pub const N_MIN: u64 = 5;
/// Samples before there is an estimate at all (unless a kill left a floor).
pub const MIN_SAMPLES: u64 = 2;
/// A run shorter than this (one second) is not a trusted measurement.
pub const MIN_WALL_US: u64 = 1_000_000;
/// Runs a floor must have gone unchallenged by the heavy mode before it halves; also the
/// span of the long maximum.
pub const DECAY_RUNS: u64 = CHUNK_RUNS * 8;
/// Days a floor must also have stood before it halves.
pub const DECAY_DAYS: u32 = 7;
/// Two own-limit kill events within this many runs make a key raise-only.
pub const KILL_WINDOW_RUNS: u64 = 256;
/// Days a key stays raise-only after its second kill event.
pub const RAISE_ONLY_DAYS: u32 = 7;
/// CPU is never booked below this: one 250 m step, so a 0.3-core action costs 500 m.
pub const CPU_MIN_MILLIS: u32 = 250;
/// What a key with no CPU sample is counted at when a pressured sample is compared with
/// its estimate: two cores, the measured mean parallelism of an unlearned action.
pub const COLD_CPU_MILLIS: u32 = 2000;
/// A CPU sample whose contention ([`Run::node_cpu_pressure_permille`], tenths of a percent)
/// is below this is always kept (unless held at a ceiling). At or above it, or with no reading, wall inflates under
/// contention and the ratio understates the demand: the sample is kept only when it
/// exceeds the key's current estimate, so it can only raise it.
pub const CPU_PRESSURE_MAX_PERMILLE: u16 = 100;
/// A key is mixed when the largest of its last 32 samples is over this many times their
/// median.
pub const MIXED_RATIO: u64 = 4;
/// A key's CPU has a heavy tail when the 90th percentile of its runs' peaks
/// ([`Run::cpu_peak_millis`]) is over this many times the median of their means: such a
/// key books that percentile of its peaks ([`Summary::cpu`]).
pub const TAIL_RATIO: u32 = 2;
/// The percentile of the peaks a heavy-tail key books, in thousandths: the 90th.
pub const TAIL_PERMILLE: usize = 900;
/// The percentile of a key's last 32 memory samples its own booking follows
/// ([`Summaries::peak_memory`](crate::Summaries::peak_memory)), in thousandths: the 95th,
/// nearest-rank. Under 20 samples that is the largest; from 20 on, the second largest, so one
/// outlier run no longer holds the key's booking up.
pub(crate) const PEAK_PERMILLE: usize = 950;
/// The fewest of a key's last 32 CPU samples that must carry a peak before its tail is
/// judged: below it the key books the median of its means, whatever its peaks.
pub const TAIL_MIN_PEAKS: usize = 5;
/// The latest day a summary accepts (about 2,700 years of days): later days are
/// clamped, and a stored row beyond it is corrupt.
pub const MAX_DAY: u32 = 1_000_000;
/// The most minutes of a lease's life the placement score looks at: `H`, the horizon of
/// the placement predicate. [`Summary::life_minutes`] is clamped to `1..=LIFE_HORIZON`.
pub const LIFE_HORIZON: u8 = 30;

/// The most samples a stored row may claim (far beyond any real count).
pub(crate) const MAX_SAMPLES: u64 = 1 << 48;

pub(crate) const MEMORY_STEP: u64 = 256 * MIB;
const CHUNK_RUNS: u64 = 32;
/// Eight full chunks and the one being filled.
const SLOTS: usize = 9;
const RING: usize = 32;
/// Days kept in `day_max`: today and the seven before.
const DAYS: usize = 8;
const SLACK: u64 = 128 * MIB;
const CPU_STEP: u32 = 250;

/// The run's CPU sample in millicores: its mean parallelism (CPU time over wall time),
/// capped to `u32` so the cast cannot truncate.
fn cpu_millis(run: &Run) -> u32 {
    let millis = u128::from(run.cpu_usage_us) * 1000 / u128::from(run.wall_us);
    millis.min(u128::from(u32::MAX)) as u32
}

/// The nearest-rank `permille`th percentile of `values` (the `ceil(permille / 1000 * n)`-th
/// smallest), after sorting them; 0 for no values.
fn percentile(values: &mut [u32], permille: usize) -> u32 {
    values.sort_unstable();
    let rank = (values.len() * permille).div_ceil(1000);
    values.get(rank.saturating_sub(1)).copied().unwrap_or(0)
}

/// The last minute (0-indexed) a run reached: its wall time in whole minutes, saturating
/// at `u8::MAX`. A run under a minute reached minute 0.
fn minutes_reached(run: &Run) -> u8 {
    u8::try_from(run.wall_us / 60_000_000).unwrap_or(u8::MAX)
}

fn round_up(value: u64, step: u64) -> u64 {
    value.div_ceil(step).saturating_mul(step)
}

/// A memory sample with the usual margin: `1.2 * sample + 128 MiB` in integer arithmetic
/// (`sample + ceil(sample / 5)`), saturating; not yet rounded to a step.
fn sized(sample: u64) -> u64 {
    sample
        .saturating_add(sample.div_ceil(5))
        .saturating_add(SLACK)
}

/// What an own-limit kill does to a floor: the new floor, and whether the kill opens a
/// new kill event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillFloor {
    /// The floor after the kill: never below the one before it.
    pub floor: u64,
    /// Whether the kill raised the floor.
    pub raised: bool,
    /// Whether the kill is a new event. A kill of an attempt booked below the floor
    /// already in force was granted before that floor was set, so it belongs to the
    /// event that set it.
    pub new_event: bool,
}

/// The floor after an own-limit kill of an attempt booked at `booked`, when `current`
/// is the floor in force (0: none), the caller's ladder retries at `raised`, and the
/// largest live node that could run the attempt has `host_memory`. The retry books the
/// ladder's next rung; with none given, the first step above the booking. The floor
/// stays strictly below `host_memory` ([`floor_ceiling`]): a booking at a node's whole
/// memory owns the node, and that stays the digest floor's business.
#[must_use]
pub fn kill_floor(current: u64, booked: u64, raised: Option<u64>, host_memory: u64) -> KillFloor {
    let above = round_up(booked.saturating_add(1), MEMORY_STEP);
    let candidate = raised
        .unwrap_or(0)
        .max(above)
        .min(floor_ceiling(host_memory));
    KillFloor {
        floor: current.max(candidate),
        raised: candidate > current,
        new_event: booked >= current,
    }
}

/// The largest key floor under a node of `host_memory` bytes: the biggest 256 MiB step
/// strictly below it (0 for a node of no memory).
#[must_use]
pub fn floor_ceiling(host_memory: u64) -> u64 {
    host_memory.saturating_sub(1) / MEMORY_STEP * MEMORY_STEP
}

/// Everything the estimator keeps for one key (about 850 bytes). Fields are crate
/// private; [`Summary::encode`] and [`Summary::decode`] are the storage form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub(crate) basis: Basis,
    /// Memory samples seen (kills and held successes included).
    pub(crate) n_total: u64,
    /// Maximum of each chunk of 32 measured (not held) memory samples, slot `chunk % 9`.
    pub(crate) chunk_max: [u64; SLOTS],
    /// Maximum of each chunk's held samples, lower bounds of a run's need (`max(peak, limit)`
    /// of a run that reached its limit, its peak plus its swap peak for one that swapped), sized
    /// with the usual margin.
    pub(crate) chunk_held: [u64; SLOTS],
    /// The last 32 samples as contributed, for the dispersion test.
    pub(crate) ring: [u64; RING],
    /// The day each `day_max` slot was last written (slot `day % 8`).
    pub(crate) day_stamp: [u32; DAYS],
    /// Largest sample of that day.
    pub(crate) day_max: [u64; DAYS],
    /// CPU samples seen.
    pub(crate) cpu_n: u64,
    /// The last 32 CPU samples, in millicores.
    pub(crate) cpu_ring: [u32; RING],
    /// For each sample in `cpu_ring`, the peak of its run in millicores; 0 when the run
    /// had none measured. Parallel to `cpu_ring`.
    pub(crate) cpu_peak_ring: [u32; RING],
    /// For each run in `ring`, the last minute (0-indexed) it reached: its wall time in
    /// whole minutes, saturating at 255. Parallel to `ring`.
    pub(crate) minute_ring: [u8; RING],
    /// The memory a retry after an own-limit kill needs at least (0: none).
    pub(crate) oom_floor: u64,
    /// `n_total` when `oom_floor` was last raised or halved.
    pub(crate) floor_anchor: u64,
    /// The day `oom_floor` was last raised or halved.
    pub(crate) floor_day: Day,
    /// `n_total` at the latest own-limit kill event.
    pub(crate) last_kill_n: Option<u64>,
    /// The key is raise-only on every day before this one.
    pub(crate) raise_only_until: Day,
    pub(crate) last_seen: Day,
}

impl Summary {
    /// An empty summary for `basis`, first seen on `now`.
    pub fn new(basis: Basis, now: Day) -> Self {
        Self {
            basis,
            n_total: 0,
            chunk_max: [0; SLOTS],
            chunk_held: [0; SLOTS],
            ring: [0; RING],
            day_stamp: [0; DAYS],
            day_max: [0; DAYS],
            cpu_n: 0,
            cpu_ring: [0; RING],
            cpu_peak_ring: [0; RING],
            minute_ring: [0; RING],
            oom_floor: 0,
            floor_anchor: 0,
            floor_day: Day(0),
            last_kill_n: None,
            raise_only_until: Day(0),
            last_seen: now,
        }
    }

    /// The instrument this summary's samples come from.
    pub fn basis(&self) -> Basis {
        self.basis
    }

    /// Memory samples seen.
    pub fn samples(&self) -> u64 {
        self.n_total
    }

    /// The memory floor an own-limit kill left (0: none).
    pub fn oom_floor(&self) -> u64 {
        self.oom_floor
    }

    /// The last day this summary saw a run.
    pub fn last_seen(&self) -> Day {
        self.last_seen
    }

    /// Whether the key may not be lowered on `now` (a clock behind the last sighting
    /// counts as the sighting).
    pub fn raise_only(&self, now: Day) -> bool {
        now.max(self.last_seen) < self.raise_only_until
    }

    /// Whether this summary holds safety state (a floor or a raise-only window) that
    /// must outlive the least-recently-seen eviction.
    pub fn holds_safety_state(&self) -> bool {
        self.oom_floor > 0 || self.raise_only(self.last_seen)
    }

    /// The maximum measured (not held) memory sample over the last 257 to 288 runs (at least 256).
    pub fn long_max(&self) -> u64 {
        self.chunk_max.iter().copied().fold(0, u64::max)
    }

    /// Whether the last 32 samples are far apart: such a key (a shared or bimodal
    /// shape) is never lowered.
    pub fn mixed(&self) -> bool {
        let kept = self.n_total.min(RING as u64) as usize;
        let mut sorted = self.ring[..kept].to_vec();
        sorted.sort_unstable();
        sorted
            .last()
            .zip(sorted.get(kept / 2))
            .is_some_and(|(max, median)| *max > median.saturating_mul(MIXED_RATIO))
    }

    /// Feeds one finished run in. Safety effects of an own-limit kill (the floor, the
    /// raise-only state) apply whatever the run's wall time. `now` never runs behind the
    /// last sighting.
    pub fn observe(&mut self, run: &Run, now: Day, bounds: &Bounds) -> Observed {
        self.last_seen = self.last_seen.max(now.min(Day(MAX_DAY)));
        let now = self.last_seen;
        let host = bounds.host_memory;
        let trusted = run.wall_us >= MIN_WALL_US;
        let memory = match run.outcome {
            Outcome::Success => match run.peak_memory_bytes {
                Some(peak) if trusted && peak > 0 => {
                    if peak >= host {
                        Err(Discard::OverCap)
                    } else {
                        self.record_success(run, peak, now, host);
                        self.decay(now);
                        Ok(())
                    }
                }
                _ => Err(Discard::Untrusted),
            },
            Outcome::OwnLimitKill => {
                // The kill's use is rarely known; it died at its limit, which is its
                // booking (a host that kills a job for memory holds it at its booking).
                let used = run.peak_memory_bytes.unwrap_or(0);
                let sample = used
                    .max(run.booked_memory_bytes)
                    .min(host.saturating_sub(1));
                if sample == 0 {
                    // Nothing booked and nothing measured: not a kill we can act on.
                    Err(Discard::Untrusted)
                } else {
                    self.record_memory(sample, false, now, minutes_reached(run));
                    self.record_kill(run, now, bounds);
                    Ok(())
                }
            }
            Outcome::NodePressureKill | Outcome::Failed | Outcome::Lost | Outcome::Cancelled => {
                Err(Discard::NotASample)
            }
        };
        let used = run.cpu_usage_us > 0;
        let cpu = if run.outcome == Outcome::Success && trusted && used {
            self.observe_cpu(run, bounds)
        } else {
            CpuSample::Skipped
        };
        Observed { memory, cpu }
    }

    /// A CPU measurement: kept when the node was quiet and the run was not held at a
    /// ceiling, otherwise only when its mean exceeds the median of the key's means (a
    /// starved or held run never overstates its demand, it only understates it). Its peak,
    /// if measured, is kept or dropped with it.
    fn observe_cpu(&mut self, run: &Run, bounds: &Bounds) -> CpuSample {
        let millis = cpu_millis(run);
        let peak = run.cpu_peak_millis.unwrap_or(0);
        let quiet = !run.cpu_held
            && run
                .node_cpu_pressure_permille
                .is_some_and(|p| p < CPU_PRESSURE_MAX_PERMILLE);
        if quiet {
            self.record_cpu(millis, peak);
            CpuSample::Kept
        } else if millis > self.median_cpu(bounds) {
            self.record_cpu(millis, peak);
            CpuSample::Raised
        } else {
            CpuSample::Dropped
        }
    }

    /// `L`, the life of an action of this key in whole minutes, from the runs in the ring:
    /// the number of leading minutes `t` (0-indexed) that at least half of them reached,
    /// clamped to `1..=LIFE_HORIZON`. A key whose runs are all under a minute, a key with
    /// no runs, and a record stored before wall time was kept have `L = 1`.
    #[must_use]
    pub fn life_minutes(&self) -> u8 {
        let kept = usize::try_from(self.n_total).map_or(RING, |n| n.min(RING));
        let runs = &self.minute_ring[..kept];
        let reached = |t: u8| runs.iter().filter(|m| **m >= t).count() * 2 >= kept;
        let life = (0..LIFE_HORIZON)
            .take_while(|t| kept > 0 && reached(*t))
            .count();
        u8::try_from(life)
            .unwrap_or(LIFE_HORIZON)
            .clamp(1, LIFE_HORIZON)
    }

    /// Whether the key has at least one CPU sample.
    pub fn has_cpu_sample(&self) -> bool {
        self.cpu_n > 0
    }

    /// A success whose peak does not show what it needed contributes a held sample, a lower
    /// bound of its need, which the estimate sizes with the usual margin: one that swapped
    /// ([`Run::swap_peak_bytes`]) had its swap in use beside its peak, so it needed at least
    /// their sum; one that reached its booking ([`Run::limit_hits`]) needed at least the
    /// booking, so it contributes the larger of that and its booking, and its class books a step
    /// above that booking next. A held sample is kept below the host. Any other success
    /// contributes its peak as measured.
    fn record_success(&mut self, run: &Run, peak: u64, now: Day, host: u64) {
        let swapped = run.swap_peak_bytes > 0;
        if run.limit_hits > 0 || swapped {
            let used = peak.saturating_add(run.swap_peak_bytes);
            let floor = if run.limit_hits > 0 {
                run.booked_memory_bytes
            } else {
                0
            };
            let need = used.max(floor).min(host.saturating_sub(1));
            self.record_memory(need, true, now, minutes_reached(run));
        } else {
            self.record_memory(peak, false, now, minutes_reached(run));
        }
    }

    fn record_memory(&mut self, sample: u64, held: bool, now: Day, minute: u8) {
        let slot = ((self.n_total / CHUNK_RUNS) % SLOTS as u64) as usize;
        if self.n_total.is_multiple_of(CHUNK_RUNS) {
            self.chunk_max[slot] = 0;
            self.chunk_held[slot] = 0;
        }
        let chunk = if held {
            &mut self.chunk_held[slot]
        } else {
            &mut self.chunk_max[slot]
        };
        *chunk = (*chunk).max(sample);
        self.ring[(self.n_total % RING as u64) as usize] = sample;
        self.minute_ring[(self.n_total % RING as u64) as usize] = minute;
        let day = (now.0 % DAYS as u32) as usize;
        if self.day_stamp[day] != now.0 {
            self.day_stamp[day] = now.0;
            self.day_max[day] = 0;
        }
        self.day_max[day] = self.day_max[day].max(sample);
        self.n_total += 1;
    }

    fn record_cpu(&mut self, millis: u32, peak: u32) {
        let slot = (self.cpu_n % RING as u64) as usize;
        self.cpu_ring[slot] = millis;
        self.cpu_peak_ring[slot] = peak;
        self.cpu_n += 1;
    }

    /// An own-limit kill. A kill of an attempt booked below the floor already in force
    /// was granted before that floor was set: it belongs to the event that set it, so
    /// 50 concurrent operations killed together are one event, not 50.
    fn record_kill(&mut self, run: &Run, now: Day, bounds: &Bounds) {
        let kill = kill_floor(
            self.oom_floor,
            run.booked_memory_bytes,
            run.raised_memory_bytes,
            bounds.host_memory,
        );
        self.oom_floor = kill.floor;
        if kill.raised || kill.new_event {
            self.floor_anchor = self.n_total;
            self.floor_day = now;
        }
        if kill.new_event {
            if self
                .last_kill_n
                .is_some_and(|p| self.n_total - p < KILL_WINDOW_RUNS)
            {
                let until = Day(now.0.saturating_add(RAISE_ONLY_DAYS));
                self.raise_only_until = self.raise_only_until.max(until);
            }
            self.last_kill_n = Some(self.n_total);
        }
    }

    /// The largest sample of today and the seven days before.
    fn recent_day_max(&self, now: Day) -> u64 {
        self.day_stamp
            .iter()
            .zip(self.day_max)
            .filter(|(stamp, _)| {
                now.0
                    .checked_sub(**stamp)
                    .is_some_and(|age| age < DAYS as u32)
            })
            .map(|(_, max)| max)
            .fold(0, u64::max)
    }

    /// Halves the floor once the heavy mode has been absent for 256 runs and 7 days; a
    /// raise-only key therefore keeps its floor.
    fn decay(&mut self, now: Day) {
        let quiet = self.n_total - self.floor_anchor >= DECAY_RUNS
            && now.0.saturating_sub(self.floor_day.0) >= DECAY_DAYS;
        let heaviest = self.long_max().max(self.recent_day_max(now));
        let light = heaviest.saturating_mul(2) < self.oom_floor;
        // No raise-only test is needed: a raise-only window ends 7 days after the
        // kill event that opened it, and that event stamped `floor_day`.
        if self.oom_floor > 0 && quiet && light {
            let half = round_up(self.oom_floor / 2, MEMORY_STEP);
            self.oom_floor = if half < MIN_MEMORY { 0 } else { half };
            self.floor_anchor = self.n_total;
            self.floor_day = now;
        }
    }

    /// Forgets the samples but keeps the safety state (floor, its day, raise-only), as
    /// an idle summary does: the floor then needs 256 new runs and 7 days to decay.
    pub(crate) fn strip_samples(&mut self) {
        let basis = self.basis;
        let kept = Self {
            oom_floor: self.oom_floor,
            floor_day: self.floor_day,
            raise_only_until: self.raise_only_until,
            ..Self::new(basis, self.last_seen)
        };
        *self = kept;
    }

    /// Whether this summary can answer: enough samples, or a floor to honour.
    pub(crate) fn usable(&self) -> bool {
        self.n_total >= MIN_SAMPLES || self.oom_floor > 0
    }

    /// The floor, kept strictly below the host.
    pub(crate) fn floor(&self, bounds: &Bounds) -> u64 {
        self.oom_floor.min(bounds.floor_ceiling())
    }

    /// The memory estimate; `raise_only` is the key's state over the level pair. It can
    /// reach the host: the caller turns that into [`crate::Fallback::OverCap`].
    pub(crate) fn memory(&self, bounds: &Bounds, raise_only: bool) -> u64 {
        let floor = self.floor(bounds);
        let sized = self.sized_memory();
        let memory = if self.mixed() {
            // Never lowered by samples and raised by a kill, but its own largest
            // recent peak with the usual margin raises it.
            bounds.default_memory.max(floor).max(sized)
        } else {
            let memory = round_up(sized.max(floor), MEMORY_STEP);
            if self.n_total < N_MIN || raise_only {
                memory.max(bounds.default_memory)
            } else {
                memory
            }
        };
        memory.max(MIN_MEMORY)
    }

    /// The largest recent peaks and held samples with the usual margin, rounded up to a
    /// step: `round_up(1.2 * max(long_max, held) + 128 MiB)`.
    fn sized_memory(&self) -> u64 {
        let held = self.chunk_held.iter().copied().fold(0, u64::max);
        round_up(sized(self.long_max().max(held)), MEMORY_STEP)
    }

    /// What the high end of the recent samples asks for whatever their number or dispersion:
    /// `round_up(max(1.2 * max(p95, held) + 128 MiB, floor))`, at least [`MIN_MEMORY`], where
    /// `p95` is the nearest-rank 95th percentile of the last 32 memory samples (kills and held
    /// samples as recorded: the largest under 20 samples, the second largest from 20 on) and
    /// `held` the largest held sample of the last 256 or more runs; `None` when the summary
    /// holds no memory sample. One outlier among 20 or more runs, and any run older than the
    /// last 32, does not hold it up; a held sample (a run that reached its booking) and a
    /// kill's floor do.
    pub(crate) fn peak_memory(&self, bounds: &Bounds) -> Option<u64> {
        let held = self.chunk_held.iter().copied().fold(0, u64::max);
        let samples = self.recent_samples();
        // The rank is 0 only for no sample, where the empty list has nothing at 0 either.
        let rank = (samples.len() * PEAK_PERMILLE).div_ceil(1000);
        samples.get(rank.saturating_sub(1)).map(|high| {
            round_up(sized((*high).max(held)), MEMORY_STEP)
                .max(round_up(self.floor(bounds), MEMORY_STEP))
                .max(MIN_MEMORY)
        })
    }

    /// What a typical recent sample asks for: the median of the last 32 memory samples (the
    /// middle one, the larger of the two middle ones for an even count; kills and held samples
    /// as recorded) with the usual margin: `round_up(1.2 * median + 128 MiB)`, at least
    /// [`MIN_MEMORY`]; `None` when the summary holds no memory sample. The figure of a key that
    /// stands in for others, so its floor counts only while its kills are frequent, while it is
    /// raise-only on `now` (two own-limit kill events within [`KILL_WINDOW_RUNS`] runs, for
    /// [`RAISE_ONLY_DAYS`] days): a kill raises the key that was killed, not every key it
    /// stands in for.
    pub(crate) fn median_memory(&self, bounds: &Bounds, now: Day) -> Option<u64> {
        let floor = if self.raise_only(now) {
            round_up(self.floor(bounds), MEMORY_STEP)
        } else {
            0
        };
        let samples = self.recent_samples();
        samples.get(samples.len() / 2).map(|median| {
            round_up(sized(*median), MEMORY_STEP)
                .max(floor)
                .max(MIN_MEMORY)
        })
    }

    /// The last 32 memory samples (fewer while fewer were seen), smallest first.
    fn recent_samples(&self) -> Vec<u64> {
        let kept = usize::try_from(self.n_total).map_or(RING, |n| n.min(RING));
        let mut samples = self.ring[..kept].to_vec();
        samples.sort_unstable();
        samples
    }

    /// The CPU estimate in millicores: [`COLD_CPU_MILLIS`] while there is no CPU sample;
    /// else, for a key with a heavy tail, the 90th percentile of its recent runs' peaks
    /// ([`TAIL_PERMILLE`]), and for any other key the median (P50) of their means; rounded up
    /// to 250 m, between [`CPU_MIN_MILLIS`] and the node's CPUs. A key has a heavy tail when
    /// at least [`TAIL_MIN_PEAKS`] of its last 32 samples carry a peak and the percentile of
    /// those peaks is over [`TAIL_RATIO`] times the median booking.
    pub(crate) fn cpu(&self, bounds: &Bounds) -> u32 {
        let median = self.median_cpu(bounds);
        let kept = self.cpu_n.min(RING as u64) as usize;
        let mut peaks: Vec<u32> = self.cpu_peak_ring[..kept]
            .iter()
            .copied()
            .filter(|peak| *peak > 0)
            .collect();
        if peaks.len() < TAIL_MIN_PEAKS {
            return median;
        }
        let tail = percentile(&mut peaks, TAIL_PERMILLE);
        if tail > median.saturating_mul(TAIL_RATIO) {
            Self::booking(tail, bounds)
        } else {
            median
        }
    }

    /// The median (P50) of the recent samples' means rounded up to 250 m, between
    /// [`CPU_MIN_MILLIS`] and the node's CPUs; [`COLD_CPU_MILLIS`] while there is no CPU
    /// sample. A pressured or held sample is kept only above it.
    fn median_cpu(&self, bounds: &Bounds) -> u32 {
        if !self.has_cpu_sample() {
            // No measurement: the key is counted at what an unlearned action costs.
            return COLD_CPU_MILLIS.min(bounds.node_cpu_millis);
        }
        let kept = self.cpu_n.min(RING as u64) as usize;
        let mut means = self.cpu_ring[..kept].to_vec();
        Self::booking(percentile(&mut means, 500), bounds)
    }

    /// `millis` rounded up to 250 m, between [`CPU_MIN_MILLIS`] and the node's CPUs.
    fn booking(millis: u32, bounds: &Bounds) -> u32 {
        let node = bounds.node_cpu_millis;
        let low = CPU_MIN_MILLIS.min(node);
        (millis.div_ceil(CPU_STEP).saturating_mul(CPU_STEP)).clamp(low, node)
    }
}
