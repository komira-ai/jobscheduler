//! What a job books in the replay: the placer's sizing over the estimator's own summaries.
//!
//! This follows `JobSizes` in `js-sched-placer/src/sizes.rs` step for step, keyed by the
//! trace's class indices instead of a job record: the CPU of `cpu_of` (the learned CPU of the
//! job's class, else of its fallback class, else of its client's whole history, held to a
//! quarter of the host and at least a quarter core; the cold CPU when none has a sample),
//! the memory of `size_of`, and the run `run_of` teaches. A run's contention is what the
//! replay modelled for it (the time it was slowed, over its wall time), so a slowed run can
//! only raise its class, as on the farm. No job is held at a CPU ceiling (no `cpu.max`). A
//! run is taught with its recorded peak only by a sizer [`Sizer::with_peaks`] made, as `run_of`
//! teaches it now; otherwise without, as it did before the heavy-tail rule.

use js_sched_placer::sizes::{DEFAULT_COLD_CPU_MILLIS, DEFAULT_COLD_MEMORY_BYTES};
use js_sizing::{
    Basis, Bounds, Day, Hint, Instance, Key, N_MIN, Outcome, PlatformClass, Run, STALE_DAYS, Shape,
    Summaries,
};

use crate::trace::{End, Job};

/// The least CPU a class books, and the step its CPU is rounded to: a quarter core.
const CPU_STEP: u64 = 250;
/// A class books at most this share of a host's cores (a quarter).
const HOST_CPU_SHARE: u64 = 4;
/// A memory peak within this many hundredths of its booking hit the booking.
const AT_LIMIT_PERCENT: u128 = 95;
/// The milliseconds of a day.
const DAY_MS: u64 = 86_400_000;

/// What a job is booked on a host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Booking {
    /// CPU, in millicores.
    pub milli: u64,
    /// Memory, in bytes.
    pub bytes: u64,
}

/// The day of `ms` milliseconds into the replay.
#[must_use]
pub fn day_of(ms: u64) -> Day {
    Day(u32::try_from(ms / DAY_MS).unwrap_or(u32::MAX))
}

/// The estimator's key of class index `index`.
fn key(index: u32) -> Key {
    Key::new(
        Instance::new("replay"),
        PlatformClass::new(""),
        Basis::Cgroup,
        Shape(u64::from(index)),
        None,
    )
}

/// The history of every class the replay has seen.
#[derive(Clone, Debug)]
pub struct Sizer {
    summaries: Summaries,
    bounds: Bounds,
    /// Whether a run is taught with its recorded peak, as the placer teaches it: a class whose
    /// peaks are a heavy tail then books them. Without, every class books the median of its
    /// runs' means, as the placer did before it read the peak.
    peaks: bool,
}

impl Sizer {
    /// A sizer for hosts of `host_milli` of CPU and `host_bytes` of bookable memory, with the
    /// placer's default cold sizes and no history.
    #[must_use]
    pub fn new(host_milli: u64, host_bytes: u64) -> Self {
        Self {
            summaries: Summaries::default(),
            bounds: Bounds {
                default_memory: DEFAULT_COLD_MEMORY_BYTES.min(host_bytes),
                host_memory: host_bytes,
                node_cpu_millis: u32::try_from(host_milli).unwrap_or(u32::MAX),
            },
            peaks: false,
        }
    }

    /// The same sizer, teaching each run with its recorded peak ([`Sizer::peaks`]).
    #[must_use]
    pub fn with_peaks(self) -> Self {
        Self {
            peaks: true,
            ..self
        }
    }

    /// What `job` books on `day`.
    #[must_use]
    pub fn book(&self, job: &Job, day: Day) -> Booking {
        Booking {
            milli: self.cpu(job, day),
            bytes: self.memory(job, day),
        }
    }

    /// The CPU `job` books on `day` (`JobSizes::cpu_of`).
    fn cpu(&self, job: &Job, day: Day) -> u64 {
        let share = u64::from(self.bounds.node_cpu_millis) / HOST_CPU_SHARE / CPU_STEP * CPU_STEP;
        let keys = [Some(job.class), job.fallback, Some(job.client)];
        let learned = keys
            .iter()
            .flatten()
            .find_map(|index| self.summaries.cpu(&key(*index), day, &self.bounds));
        learned
            .map_or(DEFAULT_COLD_CPU_MILLIS.min(share), u64::from)
            .min(share)
            .max(CPU_STEP)
    }

    /// The memory `job` books on `day` (`JobSizes::size_of`): a class with at least
    /// [`N_MIN`] fresh runs books its own estimate; any other books the largest of its
    /// fallback's (else its client's, else the cold) memory, its own runs' and its floor.
    fn memory(&self, job: &Job, day: Day) -> u64 {
        let bounds = &self.bounds;
        let own = key(job.class);
        let fresh = self
            .summaries
            .get(&own)
            .filter(|summary| day.0.saturating_sub(summary.last_seen().0) <= STALE_DAYS);
        if fresh.is_some_and(|summary| summary.samples() >= N_MIN) {
            return self
                .summaries
                .estimate(&own, day, bounds, &Hint::default())
                .map_or(bounds.host_memory, |estimate| estimate.memory_bytes);
        }
        let peak = |index: u32| {
            let peak = self.summaries.peak_memory(&key(index), day, bounds);
            peak.filter(|memory| *memory < bounds.host_memory)
        };
        let base = job
            .fallback
            .and_then(peak)
            .or_else(|| peak(job.client))
            .unwrap_or(bounds.default_memory);
        let own_peak = self.summaries.peak_memory(&own, day, bounds).unwrap_or(0);
        if own_peak >= bounds.host_memory {
            return bounds.host_memory;
        }
        base.max(own_peak)
            .max(self.summaries.key_floor(&own, bounds))
    }

    /// Teaches the run of `job` that lasted `wall_ms`, of which `slowed_ms` were lost to
    /// contention, booked as `booked`, ending on `day` (`JobSizes::observe` and `run_of`).
    pub fn learn(
        &mut self,
        job: &Job,
        (wall_ms, slowed_ms): (u64, u64),
        booked: Booking,
        day: Day,
    ) {
        let outcome = match job.end {
            End::Ok => Outcome::Success,
            End::Failed => Outcome::Failed,
            End::Memory => Outcome::OwnLimitKill,
        };
        let killed = outcome == Outcome::OwnLimitKill;
        let peak = u64::from(job.memory_mib) << 20;
        let at_limit = booked.bytes > 0
            && u128::from(peak) * 100 >= u128::from(booked.bytes) * AT_LIMIT_PERCENT;
        let wall_us = wall_ms.saturating_mul(1000);
        let permille = u128::from(slowed_ms) * 1000 / u128::from(wall_ms.max(1));
        let run = Run {
            outcome,
            wall_us,
            peak_memory_bytes: Some(peak),
            booked_memory_bytes: booked.bytes,
            raised_memory_bytes: killed.then(|| booked.bytes.saturating_mul(2)),
            limit_hits: u64::from(at_limit),
            swap_peak_bytes: 0,
            cpu_usage_us: job.cpu_ms.saturating_mul(1000),
            cpu_peak_millis: job.peak_milli.filter(|_| self.peaks),
            node_cpu_pressure_permille: Some(u16::try_from(permille).unwrap_or(u16::MAX)),
            cpu_held: false,
        };
        let mut taught = vec![job.class];
        taught.extend(job.fallback);
        if !killed {
            taught.push(job.client);
        }
        for index in taught {
            let _ = self.summaries.observe(&key(index), &run, day, &self.bounds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::tests::job;

    const GIB: u64 = 1 << 30;

    fn farm() -> Sizer {
        Sizer::new(88_000, 251 * GIB)
    }

    /// Teaches `sizer` `runs` quiet runs of `job` of a minute each at `milli` mean CPU.
    fn teach(sizer: &mut Sizer, job: &Job, milli: u64, runs: usize) {
        let job = Job {
            cpu_ms: milli * 60,
            ..job.clone()
        };
        for _ in 0..runs {
            sizer.learn(&job, (60_000, 0), Booking::default(), Day(0));
        }
    }

    #[test]
    fn a_class_with_no_history_books_the_cold_cpu_and_memory() {
        let sizer = farm();
        assert_eq!(
            sizer.book(&job(0, 1, 1, 7), Day(0)),
            Booking {
                milli: 22_000,
                bytes: 4 * GIB
            }
        );
        // On a host of 16 cores the cold CPU is a quarter of it.
        let small = Sizer::new(16_000, 64 * GIB);
        assert_eq!(small.book(&job(0, 1, 1, 7), Day(0)).milli, 4000);
        assert_eq!(day_of(DAY_MS * 3 + 1), Day(3));
        assert_eq!(day_of(u64::MAX), Day(u32::MAX));
    }

    #[test]
    fn a_class_books_the_median_of_its_runs_mean_cpu() {
        let mut sizer = farm();
        let compile = job(0, 1, 1, 1);
        // Two runs at 1.9 cores and one at 9: the median is 1.9, rounded up to 2.
        teach(&mut sizer, &compile, 1900, 2);
        teach(&mut sizer, &compile, 9000, 1);
        assert_eq!(sizer.book(&compile, Day(0)).milli, 2000);
    }

    #[test]
    fn a_class_falls_back_to_its_fallback_then_its_client() {
        let mut sizer = farm();
        let new = Job {
            fallback: Some(2),
            client: 3,
            ..job(0, 1, 1, 1)
        };
        // Only the client has history: the client's CPU.
        teach(&mut sizer, &job(0, 1, 1, 3), 500, 1);
        assert_eq!(sizer.book(&new, Day(0)).milli, 500);
        // The fallback class has history: its CPU.
        teach(&mut sizer, &job(0, 1, 1, 2), 3000, 1);
        assert_eq!(sizer.book(&new, Day(0)).milli, 3000);
        // A huge class is held at a quarter of the host, a tiny one at a quarter core.
        teach(&mut sizer, &job(0, 1, 1, 4), 80_000, 1);
        assert_eq!(sizer.book(&job(0, 1, 1, 4), Day(0)).milli, 22_000);
        teach(&mut sizer, &job(0, 1, 1, 5), 10, 1);
        assert_eq!(sizer.book(&job(0, 1, 1, 5), Day(0)).milli, 250);
    }

    #[test]
    fn a_slowed_run_only_raises_its_class() {
        let mut sizer = farm();
        let compile = job(0, 1, 1, 1);
        teach(&mut sizer, &compile, 2000, 1);
        // Half its wall time lost to contention, and lower: dropped.
        let slow = Job {
            cpu_ms: 60,
            ..compile.clone()
        };
        sizer.learn(&slow, (60_000, 30_000), Booking::default(), Day(0));
        assert_eq!(sizer.book(&compile, Day(0)).milli, 2000);
        // Slowed but higher: kept.
        let high = Job {
            cpu_ms: 5000 * 60,
            ..compile.clone()
        };
        sizer.learn(&high, (60_000, 30_000), Booking::default(), Day(0));
        sizer.learn(&high, (60_000, 30_000), Booking::default(), Day(0));
        assert_eq!(sizer.book(&compile, Day(0)).milli, 5000);
    }

    #[test]
    fn memory_follows_the_class_once_it_has_five_runs_and_the_fallback_before() {
        let mut sizer = farm();
        let class = Job {
            fallback: Some(2),
            memory_mib: 1024,
            ..job(0, 1, 1, 1)
        };
        // A fallback class that peaked at 2 GiB: 1.2 x 2 GiB + 128 MiB, rounded up to 256
        // MiB, is 2.75 GiB.
        let other = Job {
            memory_mib: 2048,
            ..job(0, 1, 1, 2)
        };
        teach(&mut sizer, &other, 1000, 1);
        assert_eq!(sizer.book(&class, Day(0)).bytes, 2816 << 20);
        // Its own first runs at 1 GiB do not lower it.
        teach(&mut sizer, &class, 1000, 4);
        assert_eq!(sizer.book(&class, Day(0)).bytes, 2816 << 20);
        // Five runs of its own: its own estimate, 1.2 x 1 GiB + 128 MiB = 1.5 GiB.
        teach(&mut sizer, &class, 1000, 1);
        assert_eq!(sizer.book(&class, Day(0)).bytes, 1536 << 20);
        // A class whose runs ask for more than the host (1.2 x 240 GiB) books the whole host,
        // before five runs and after.
        let big = Job {
            memory_mib: 240 * 1024,
            ..job(0, 1, 1, 9)
        };
        teach(&mut sizer, &big, 1000, 1);
        assert_eq!(sizer.book(&big, Day(0)).bytes, 251 * GIB);
        teach(&mut sizer, &big, 1000, 4);
        assert_eq!(sizer.book(&big, Day(0)).bytes, 251 * GIB);
    }

    #[test]
    fn a_memory_kill_raises_its_class_and_not_its_client() {
        let mut sizer = farm();
        let killed = Job {
            end: End::Memory,
            memory_mib: 4096,
            client: 3,
            ..job(0, 1, 1, 1)
        };
        let booked = Booking {
            milli: 1000,
            bytes: 4 * GIB,
        };
        sizer.learn(&killed, (60_000, 0), booked, Day(0));
        assert!(sizer.book(&killed, Day(0)).bytes > 4 * GIB);
        assert_eq!(sizer.book(&job(0, 1, 1, 3), Day(0)).bytes, 4 * GIB);
        // A failed run teaches nothing of CPU.
        let failed = Job {
            end: End::Failed,
            cpu_ms: 600_000,
            ..job(0, 1, 1, 4)
        };
        sizer.learn(&failed, (60_000, 0), booked, Day(0));
        assert_eq!(sizer.book(&failed, Day(0)).milli, 22_000);
    }
}
