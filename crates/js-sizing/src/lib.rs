//! The size estimator: predicts the CPU and memory an action will use from what
//! earlier runs of similar actions used.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The lists in `clippy.toml` enforce it.
//! Time enters as a [`Day`] argument; the crate has its own small input types
//! ([`Run`], [`Bounds`], [`Hint`]) and depends on no other crate. A server adapts its
//! run records to these types and keeps the [`Summaries`] map.
//!
//! # What it computes
//!
//! A [`Summaries`] map holds one small [`Summary`] per [`Key`]. Every finished run is
//! fed to [`Summaries::observe`]; [`Summaries::estimate`] answers with an [`Estimate`]
//! or a [`Fallback`] (the caller then books what it books today).
//!
//! * **Key.** `(instance, platform class, basis, shape, cfg)`. `basis` (cgroup or Mac
//!   footprint) is part of the key, so the two instruments never feed each other. Level
//!   L1 is the full key; L2 drops `cfg`. A run updates both; the estimate uses the first
//!   level with at least [`N_MIN`] samples, else the first with at least
//!   [`MIN_SAMPLES`], else there is no estimate.
//! * **Samples.** A success with a non-zero measured peak (zero is not a measurement)
//!   and a wall time of at least one second, or an own-limit memory kill. A kill is a
//!   sample of at least its booking, the limit it died at (a host that kills a job for
//!   memory holds it at its booking). Failed non-OOM runs, lost or
//!   cancelled runs and node-pressure kills are not samples. A success that reached its
//!   booking ([`Run::limit_hits`] above zero: the caller judges it) needed at least the
//!   booking: it is a held sample of `max(peak, booked)`, so its key books a step above the
//!   booking next, and steps again while its runs keep reaching it. A success that swapped
//!   ([`Run::swap_peak_bytes`] above zero) had part of what it held pushed out of memory: it
//!   is a held sample of its peak plus its swap peak (the most it can have held at once) (and, when it also reached its booking, of at least
//!   the booking). Held samples are sized with the same margin as measured
//!   ones, and a held sample is kept below the host, so its step never passes the host
//!   (one that reaches it is [`Fallback::OverCap`], below). Every other success's peak is a
//!   measurement whatever its booking. CPU samples are successful runs with CPU time that started below
//!   [`CPU_PRESSURE_MAX_PERMILLE`] node CPU pressure and were not held at a CPU
//!   ceiling ([`Run::cpu_held`]).
//! * **Memory.** `round_up_256MiB(max(1.2 * max(M, held) + 128 MiB, oom_floor))` in integer
//!   arithmetic (`M + ceil(M / 5)`, so results match on every architecture), where `M`
//!   is the maximum measured (not held) sample over the last 256 or more runs (nine chunk
//!   maxima of 32 runs: the eight latest full chunks plus the current one, a span of
//!   257 to 288 runs) and `held` the largest held one; at least 1 GiB. The
//!   statistic is the maximum, not a percentile. With fewer than [`N_MIN`] samples, or
//!   while raise-only, the estimate may only raise (it is at least the default
//!   booking). A *mixed* key (the largest of its last 32 samples over four times their
//!   median) is never lowered by samples and raised by kills: its estimate is the larger
//!   of the default booking, its floor and its own sized estimate
//!   `round_up_256MiB(1.2 * max(M, held) + 128 MiB)`, so it is never booked below
//!   its own largest recent peak with the margin. [`Summaries::peak_memory`] answers
//!   what the high end of a key's recent samples asks for whatever their number or
//!   dispersion: the nearest-rank 95th percentile of its last 32 samples (the largest under
//!   20 samples, the second largest from 20 on, so one outlier run does not hold it up), at
//!   least its largest held sample of the 256 or more runs above and its floor, with the same
//!   margin; [`Summaries::median_memory`] what its typical recent sample asks for (the median
//!   of its last 32 samples, the larger middle one for an even count, with the same margin),
//!   the figure of a key that stands in for another one, which takes the key's floor only
//!   while the key is raise-only (its kills are frequent: a kill raises the key killed, not
//!   every key it stands in for).
//! * **Never a whole host.** A booking at or above the memory of the largest matching
//!   live node ([`Bounds::host_memory`]) owns the host. The crate's key floors stay
//!   strictly below it, and an estimate (with the client's hint) that would reach it is
//!   [`Fallback::OverCap`]: the caller books cold and the ladder and digest floors
//!   decide.
//! * **CPU.** `round_up_250m(P50(sample))`, clamped to `[250 m, node cpus]`: the
//!   nearest-rank median of the last 32 kept samples, where a run's sample is its mean,
//!   `cpu_usage / wall` (a 0.3-core action costs 500 m); [`COLD_CPU_MILLIS`] with no CPU
//!   sample. Each sample keeps its run's peak over a short window
//!   ([`Run::cpu_peak_millis`]) beside it when measured. A key with a **heavy tail** (at
//!   least [`TAIL_MIN_PEAKS`] of its last 32 samples carry a peak, and the nearest-rank
//!   90th percentile of those peaks, [`TAIL_PERMILLE`], is over [`TAIL_RATIO`] times the
//!   median booking) books `round_up_250m(P90(peak))` instead, with the same clamp: its runs
//!   burst far above their mean, and a host filled by its median runs them over its cores.
//!   A sample taken at or
//!   above [`CPU_PRESSURE_MAX_PERMILLE`] node pressure, with no pressure reading, or
//!   from a run held at a ceiling ([`Run::cpu_held`]: its mean is a lower bound), is
//!   kept (with its peak) only when its mean exceeds the median of the key's means
//!   ([`COLD_CPU_MILLIS`] for a key with no sample), so under load an estimate can only
//!   rise. [`Observed::cpu`] says which happened and [`Summaries::cpu_counts`] counts it.
//! * **Hints.** The final memory and CPU are the larger of the estimate and the client's
//!   hint: an estimate never gives less than the client asked for.
//! * **Safety.** An own-limit kill sets `oom_floor` to at least the next rung the caller
//!   books on the retry (and above the booking it died at). Kills are counted as
//!   events: a kill of an attempt booked below the floor already in force was granted
//!   before it and belongs to the same event, so a burst of concurrent kills is one
//!   event. The floor halves only after 256 runs and 7 days since it was last set, and
//!   only while the largest sample of the last 256 runs and of the last 8 days is
//!   under half of it; a heavy mode that recurs never lets it go. Two kill events
//!   within 256 runs make the key raise-only for 7 days (floor and samples kept, never
//!   below the default, no decay, no fallback to "no estimate"). Safety state does not
//!   expire by idleness: a summary idle over 60 days gives [`Fallback::Stale`] for
//!   its samples, but [`Summaries::key_floor`] still returns its floor and
//!   [`Summaries::expire`] keeps the floor and drops only the samples. Keys holding a
//!   floor live in a separate table the least-recently-seen eviction never touches
//!   (per-instance quota on the main table); if that table is full the key stays in
//!   the main table and [`Summaries::unpinned`] counts the refusal. Missing, stale or
//!   undecodable summaries are a [`Fallback`], never a guess. Time is an input and
//!   never runs backward inside a summary.
//! * **Caller duties and known limits.**
//!   1. Apply `max(key_floor, ..)` to the booking on every path, including
//!      [`Fallback::Stale`], [`Fallback::OverCap`] and any other fallback:
//!      [`Summaries::key_floor`] still returns the floor there.
//!   2. The crate lowers an estimate below the default once a key has [`N_MIN`] samples,
//!      unless the key is raise-only or mixed. "Never lower below the default" in a
//!      phase that must not is therefore the caller's guarantee: the adapter clamps the
//!      estimate to at least the default or passes it raise-only.
//!   3. A floor never leaves by idleness (`expire` strips samples only), so the safety
//!      table fills slowly, over months. [`Summaries::unpinned`] counts the refusals and
//!      is the signal. An idle horizon for floors (for example 365 days) is a follow-up,
//!      not done here. The safety table holds at most a quarter of its slots per
//!      instance.
//!   4. [`Summaries::insert_encoded`] replaces the in-memory row unconditionally; do not
//!      load a stored row over a newer in-memory floor.
//! * **Calibration.** The test `the_kill_rate_stays_within_the_declared_bound` draws
//!   peaks from a lognormal around 1 GiB (sigma in rows) and from the same with a 3%
//!   tail four times larger, gives the crate `n` samples, and counts how often the next
//!   run needs more than the build-host limit (1.5 x booking + 512 MiB) of the booking it gets, in
//!   10,000 trials per cell. Declared bound, per mille:
//!
//!   | sigma, tail | n=3 | n=4 | n=5 | n=10 | n=32 |
//!   |---|---|---|---|---|---|
//!   | 0.1 | 1 | 1 | 1 | 1 | 1 |
//!   | 0.3 | 1 | 1 | 2 | 2 | 1 |
//!   | 0.6 | 2 | 2 | 18 | 8 | 3 |
//!   | 1.0 | 35 | 34 | 63 | 38 | 36 |
//!   | 0.1, 3% x4 | 1 | 1 | 30 | 27 | 12 |
//!   | 0.3, 3% x4 | 2 | 2 | 21 | 17 | 9 |
//!   | 0.6, 3% x4 | 10 | 8 | 32 | 18 | 9 |
//!   | 1.0, 3% x4 | 43 | 41 | 68 | 45 | 43 |
//!
//!   The jump at n=5 is the first lowering below the default (a tail run after five
//!   quiet ones); the n=3 and n=4 columns are the default booking.
//! * **Storage.** [`Summary::encode`] writes a versioned, checksummed, fixed-size
//!   record (counters and days beyond sane bounds are corrupt); [`Summary::decode`] returns `None` for anything else, and a key whose row
//!   does not decode starts cold.
//!
//! # Profiles
//!
//! A [`Profile`] learns what the runs of one class used over their first two minutes and
//! after. It keeps the class's last [`MAX_RUNS`] runs, each as 2 s slots for ages 0 to
//! 128 s (a CPU slot is the mean of its seconds, a memory slot the largest) and the sum
//! and seconds of the run past 128 s. Nothing else is stored: every statistic (the
//! level, a slot's mean and 90th percentile, the tail, the histogram of all samples in
//! [`BUCKETS`] log-spaced buckets) is computed on demand, with each run weighted by the
//! age of its end, halving every six hours ([`HALF_LIFE_MS`]). All arithmetic is integer.
//!
//! [`Profile::check_and_learn`] first checks a run against the profile as it stands. With
//! at least [`MIN_RUNS`] kept runs it scores the run twice, in unit-seconds: against the
//! curve (a slot's mean when [`MIN_SLOT_RUNS`] runs reached it, else the level) and
//! against the flat level. The last [`CHECK_RUNS`] scores are the profile's window, and
//! the profile is [`Profile::shaped`] when the window is full and the curve's error is at
//! most [`FIT_PERMILLE`] per mille of the flat level's. Then it learns the run: a clean
//! run always, an unclean one (slowed by contention or held at a ceiling) only when its
//! mean is above the level, so it can only raise its class. [`series_agrees`] says
//! whether a recorded CPU series matches the run's exact CPU time within 5%.
//!
//! A kept run is its slots (4 bytes each, at most 64), a few counters and its end time:
//! about 330 bytes. A profile of 64 runs is about 21 KiB, and the self-check
//! window adds 16 pairs of counters.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod codec;
mod curve;
mod key;
mod profile;
mod run;
mod store;
mod summary;
#[cfg(test)]
mod tests;

pub use curve::{
    CURVE_SLOTS, Checks, Clamp, Curve, MAX_OVERRUNS, MIN_USABLE_SERIES, SeriesWindow, TAIL_SLOT,
    age_slot,
};
pub use key::{Basis, Cfg, Day, Instance, Key, Level, PlatformClass, Shape};
pub use profile::{
    BUCKETS, CHECK_RUNS, CPU_UNIT, Checked, Combine, FIT_PERMILLE, HALF_LIFE_MS, MAX_RUNS,
    MEMORY_UNIT, MIN_RUNS, MIN_SLOT_RUNS, OVERRUN_PERMILLE, Profile, RunSeries, SLOT_SECS,
    START_SLOTS, Unit, Window, series_agrees,
};
pub use run::{Bounds, CpuSample, Discard, Hint, Observed, Outcome, Run};
pub use store::{
    CpuCounts, Estimate, Fallback, MAX_KEYS, MAX_KEYS_PER_INSTANCE, MAX_PINNED, STALE_DAYS,
    Summaries,
};
pub use summary::{
    COLD_CPU_MILLIS, CPU_MIN_MILLIS, CPU_PRESSURE_MAX_PERMILLE, DECAY_DAYS, DECAY_RUNS,
    KILL_WINDOW_RUNS, KillFloor, LIFE_HORIZON, MAX_DAY, MIN_MEMORY, MIN_SAMPLES, MIN_WALL_US,
    MIXED_RATIO, N_MIN, RAISE_ONLY_DAYS, Summary, TAIL_MIN_PEAKS, TAIL_PERMILLE, TAIL_RATIO,
    floor_ceiling, kill_floor,
};

/// One mebibyte.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;
