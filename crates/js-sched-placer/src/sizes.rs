//! The sizes of the jobs that book nothing: a job whose needs leave the CPU or the memory at
//! zero is booked, for each dimension it leaves at zero, what the history of its class says
//! (`docs/design/sizing.md`, section 7.7), through the estimator of [`js_sizing`].
//!
//! # The class
//!
//! A job's class is the value of its label [`SIZE_CLASS_LABEL`] when it has one, else its
//! image's name (without the digest, so a rebuilt image keeps the history) and its command. The
//! arguments are not part of it: a client's arguments name what one run works on (an action's
//! digest, a lease, a deadline, a grant), which no other run shares, so a class that held them
//! would be a class of one run and never learn. A client that runs different work under one
//! image and command names each work's class in the label. The estimator's key is that class's
//! SHA-256 (its first eight bytes), under the client that submitted the job (two clients never
//! share a class) and the job's platform (its `os` and `arch`, as it names them), on the cgroup
//! basis.
//!
//! # The size
//!
//! [`JobSizes::sized`] books, for each dimension the job leaves at zero:
//!
//! - **Learned.** The estimate of [`js_sizing::Summaries::estimate`] for the class: the memory
//!   is `1.2 x` the largest peak of the class's recent runs plus 128 MiB (at least 1 GiB, never
//!   lower than [`Bounds::default_memory`] before five runs, never below a memory kill's
//!   floor); the CPU is the 90th percentile of the CPU the runs used per second of wall time,
//!   rounded up to a quarter core.
//! - **Cold.** A class with no estimate yet ([`js_sizing::Fallback::Missing`], `Cold` or
//!   `Stale`) books the cold size: a quarter of the most bookable memory and a quarter of the
//!   most cores a known host has ([`Bounds`] of [`bounds_of`]), and at least the class's memory floor.
//! - **Too large.** A class whose estimate reaches the most bookable memory a host has
//!   ([`js_sizing::Fallback::OverCap`]) books all of that memory, and the cold CPU: it waits
//!   for a host with nothing else on it.
//!
//! A dimension the job names is booked as named, and the history never changes it. The size is
//! decided at each pass, not stored on the job: the job's own needs stay as submitted, so a
//! retried submit with the same id is still the same job, and a queued job is booked by the
//! history as it stands when it is placed. Its allocation records what it was booked.
//!
//! The booking is the job's limit on its host (a contained job's `memory.max` and `cpu.max`
//! are what it books), so the CPU slack and the memory percent of its host bound it as they
//! bound every booking, and nothing here books above them.
//!
//! # The history
//!
//! The estimator learns from a job when its allocation is freed: [`JobSizes::freed`] names
//! the jobs whose live allocations are gone since the last pass, and
//! [`JobSizes::observe`] feeds each one that ended with a result, with what its allocation
//! booked. At a server's start, [`JobSizes::observe`] is fed every ended job the job store
//! still holds, in the order they ended ([`crate::place::SchedPlacer::learn_ended`]): the job
//! records are the history, and nothing else is stored.
//!
//! A run is one sample of [`js_sizing::Run`]:
//!
//! - its outcome is its result's: an exit with code 0 is a success, any other exit, a wall
//!   limit or a failed input a failure (which teaches the memory nothing), a cancel a cancel,
//!   and a stop for memory a kill at its own limit (a contained job's memory limit is its
//!   booking), which raises the class's floor to twice the booking it died at;
//! - its peak memory and CPU time are what its host reported; its wall time is from its start
//!   to its end;
//! - a success whose peak came within 5% of its booking counts as having hit its limit, so it
//!   can hold an estimate at the booking but never lower it (the estimator's own censoring
//!   rule assumes the build-host limit, half as much again above the booking);
//! - its CPU counts as measured at no node pressure: a contained job's CPU is held to its
//!   booking by its own `cpu.max`, and the CPU slack bounds what its host books, so what it
//!   used is what it was given, not what a busy host left it.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    AllocationRecord, AllocationStep, Booked, IdSource, JobId, JobRecord, JobStore, JobStoreError,
    MAX_PAGE, Needs, ResultReason,
};
use js_sizing::{
    Basis, Bounds, Day, Fallback, Hint, Instance, Key, Outcome, PlatformClass, Run, Shape,
    Summaries,
};
use js_types::sched::FarmTime;
use sha2::{Digest as _, Sha256};
use tracing::warn;

use crate::place::SchedPlacer;

/// The label whose value names a job's class (`size-class`): the name of the work, such as the
/// build target it runs. A client that runs different work under one image and command names
/// it, so that each work has its own history; without it, every run of an image and command
/// shares one.
pub const SIZE_CLASS_LABEL: &str = "size-class";

/// The share a cold class books of the most cores and the most bookable memory a host has: a
/// quarter.
const COLD_SHARE: u64 = 4;

/// The CPU step a cold booking is rounded down to, and its least: a quarter core.
const CPU_STEP: u64 = 250;

/// The memory step a cold booking is rounded down to: 256 MiB.
const MEMORY_STEP: u64 = 256 << 20;

/// The milliseconds of a day.
const DAY_MILLIS: u64 = 86_400_000;

/// A peak within this many hundredths of its booking hit the booking.
const AT_LIMIT_PERCENT: u64 = 95;
/// The history of every class the server has seen, and the job allocations live at the last
/// pass.
#[derive(Debug, Default)]
pub struct JobSizes {
    summaries: Summaries,
    /// Each job whose allocation was live at the last pass, with what it booked.
    live: BTreeMap<JobId, Booked>,
    /// The bounds of the hosts the engine knows, from the last walk that knew one: what the
    /// sizes and the runs are taken against. `None` before any.
    bounds: Option<Bounds>,
    /// The runs observed before any bounds were known, in the order they came: taught when
    /// the first bounds come.
    pending: Vec<(Key, Run, Day)>,
}

impl JobSizes {
    /// No history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The estimator's key of `job`'s class.
    #[must_use]
    pub fn key_of(job: &JobRecord) -> Key {
        let spec = &job.spec;
        let client = job.client.as_ref().map_or("", |client| client.as_str());
        let constraints = &spec.constraints;
        let os = constraints.os.as_deref().unwrap_or("");
        let arch = constraints.arch.as_deref().unwrap_or("");
        Key::new(
            Instance::new(client),
            PlatformClass::new(format!("{os}/{arch}")),
            Basis::Cgroup,
            Shape(class_hash(job)),
            None,
        )
    }

    /// The bounds the next sizes and runs are taken against, when `bounds` knows a host (else
    /// the last ones stay); the runs observed before any bounds were known are taught now.
    pub fn set_bounds(&mut self, bounds: Option<Bounds>) {
        let Some(bounds) = bounds else {
            return;
        };
        self.bounds = Some(bounds);
        for (key, run, day) in std::mem::take(&mut self.pending) {
            let _ = self.summaries.observe(&key, &run, day, &bounds);
        }
    }

    /// `job` as the walk books it on `day`: a job whose needs leave the CPU or the memory at
    /// zero, with each of them sized from its class's history (the module docs); any other job,
    /// and every job while no bounds are known (no host to place it on), as it is.
    #[must_use]
    pub fn sized(&self, job: &JobRecord, day: Day) -> JobRecord {
        let (Needs::Shape(resources), Some(bounds)) = (&job.spec.needs, &self.bounds) else {
            return job.clone();
        };
        let (cpu, memory) = (resources.cpu_millis(), resources.memory_bytes());
        if (cpu != 0) & (memory != 0) {
            return job.clone();
        }
        let (sized_cpu, sized_memory) = self.size_of(job, day, bounds);
        let cpu = if cpu == 0 { sized_cpu } else { cpu };
        let memory = if memory == 0 { sized_memory } else { memory };
        let mut sized = job.clone();
        let resources = resources
            .clone()
            .with_cpu_millis(cpu)
            .with_memory_bytes(memory);
        sized.spec.needs = Needs::Shape(resources);
        sized
    }

    /// The CPU and memory the history of `job`'s class books on `day`, under `bounds`.
    fn size_of(&self, job: &JobRecord, day: Day, bounds: &Bounds) -> (u64, u64) {
        let key = Self::key_of(job);
        let cold_cpu = u64::from(bounds.node_cpu_millis) / COLD_SHARE / CPU_STEP * CPU_STEP;
        let cold_cpu = cold_cpu.max(CPU_STEP);
        match self.summaries.estimate(&key, day, bounds, &Hint::default()) {
            Ok(estimate) => (u64::from(estimate.cpu_millis), estimate.memory_bytes),
            Err(Fallback::OverCap) => (cold_cpu, bounds.host_memory),
            // Missing, cold or stale (a summary that does not decode is never stored here).
            Err(_) => {
                let floor = self.summaries.key_floor(&key, bounds);
                (cold_cpu, bounds.default_memory.max(floor))
            }
        }
    }

    /// The jobs whose allocations were live at the last call and are not in `live` now, each
    /// with what it booked; `live` is kept for the next call.
    pub fn freed(&mut self, live: &[AllocationRecord]) -> Vec<(JobId, Booked)> {
        let now: BTreeMap<JobId, Booked> = live
            .iter()
            .map(|allocation| (allocation.job.clone(), allocation.booked))
            .collect();
        let before = std::mem::replace(&mut self.live, now);
        before
            .into_iter()
            .filter(|(job, _)| !self.live.contains_key(job))
            .collect()
    }

    /// Counts the allocation of `job`, booking `booked`, live from the pass that placed it, so
    /// that a job placed and freed between two passes is still named freed by the next one.
    pub fn placed(&mut self, job: &JobId, booked: Booked) {
        self.live.insert(job.clone(), booked);
    }

    /// Teaches the history of `job`'s class the run of `job`, which ended booked as `booked`,
    /// on the day `job` ended (held until bounds are known). A job that has not ended, ended
    /// with no result (lost, failed to start), booked the whole host or never started teaches
    /// nothing. Answers whether the run was taken.
    pub fn observe(&mut self, job: &JobRecord, booked: Booked) -> bool {
        let Some((run, day)) = run_of(job, booked) else {
            return false;
        };
        self.pending.push((Self::key_of(job), run, day));
        self.set_bounds(self.bounds);
        true
    }
}

/// The run of `job`, booked as `booked`, as the estimator takes it (the module docs), with the
/// day it ended; `None` for a job that teaches nothing.
fn run_of(job: &JobRecord, booked: Booked) -> Option<(Run, Day)> {
    let Booked::Shape { memory_bytes, .. } = booked else {
        return None;
    };
    let result = job.result.as_ref()?;
    let (started, ended) = (result.timing.started_at?, result.timing.ended_at);
    let outcome = match result.reason {
        ResultReason::Exit if result.exit_code == Some(0) => Outcome::Success,
        ResultReason::Exit | ResultReason::WallLimit | ResultReason::InputFailed => Outcome::Failed,
        ResultReason::Memory => Outcome::OwnLimitKill,
        ResultReason::Cancelled => Outcome::Cancelled,
    };
    let killed = outcome == Outcome::OwnLimitKill;
    let peak = result.usage.map(|usage| usage.peak_memory_bytes);
    let cpu = result.usage.map_or(0, |usage| micros(usage.cpu_time));
    // At the limit: a peak within 5% of a booking (a booking of 0 has no limit).
    let at_limit = peak.is_some_and(|peak| {
        let peak = u128::from(peak) * 100;
        (memory_bytes > 0) & (peak >= u128::from(memory_bytes) * u128::from(AT_LIMIT_PERCENT))
    });
    let run = Run {
        outcome,
        wall_us: micros(ended.since(started)),
        peak_memory_bytes: peak,
        booked_memory_bytes: memory_bytes,
        raised_memory_bytes: killed.then(|| memory_bytes.saturating_mul(2)),
        limit_hits: u64::from(at_limit),
        swap_peak_bytes: 0,
        cpu_usage_us: cpu,
        node_cpu_pressure_permille: Some(0),
    };
    Some((run, day_of_millis(millis(ended.as_duration()))))
}

/// The bounds of hosts of `capacities` (each the CPU and the bookable memory a host offers):
/// the most memory and the most cores a host has (each on its own), with a cold booking of a
/// quarter of that memory,
/// rounded down to 256 MiB and at least 1 GiB. `None` with no host that offers memory.
#[must_use]
pub fn bounds_of(
    capacities: impl IntoIterator<Item = js_types::sched::Resources>,
) -> Option<Bounds> {
    let (cpu, memory) = capacities
        .into_iter()
        .fold((0u64, 0u64), |(cpu, memory), capacity| {
            (
                cpu.max(capacity.cpu_millis),
                memory.max(capacity.memory_bytes),
            )
        });
    let cold = (memory / COLD_SHARE / MEMORY_STEP * MEMORY_STEP).max(js_sizing::GIB);
    (memory > 0).then(|| Bounds {
        default_memory: cold,
        host_memory: memory,
        node_cpu_millis: u32::try_from(cpu).unwrap_or(u32::MAX),
    })
}

/// The estimator's day of `now`: whole days since the Unix epoch.
#[must_use]
pub fn day_of(now: FarmTime) -> Day {
    day_of_millis(now.as_millis())
}

/// The day of `millis` since the Unix epoch.
fn day_of_millis(millis: u64) -> Day {
    Day(u32::try_from(millis / DAY_MILLIS).unwrap_or(u32::MAX))
}

/// The name of `job`'s class, the history key a usage row records: its `size-class` label as
/// it is, else `run-` and the 16 hex digits of the hash of its image name and command (never
/// its image digest, its id or its args, which change with every build or run).
#[must_use]
pub fn class_name(job: &JobRecord) -> String {
    match job.spec.labels.get(SIZE_CLASS_LABEL) {
        Some(class) => class.to_owned(),
        None => format!("run-{:016x}", class_hash(job)),
    }
}

/// The first 8 bytes, big-endian, of the SHA-256 of `job`'s class text: `label`, NUL and its
/// `size-class` label, else `run`, NUL, its image name, NUL and its command joined by NUL.
fn class_hash(job: &JobRecord) -> u64 {
    let spec = &job.spec;
    let class = match spec.labels.get(SIZE_CLASS_LABEL) {
        Some(class) => format!("label\0{class}"),
        None => {
            let image = spec.image.as_ref().map_or("", |image| image.name());
            format!("run\0{image}\0{}", spec.command.join("\0"))
        }
    };
    let digest = Sha256::digest(class.as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// `duration` in whole milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// `duration` in whole microseconds, saturating.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl SchedPlacer {
    /// Teaches the history the jobs whose allocations `live` (the live allocations a pass read)
    /// no longer holds since the last pass: each is read from `jobs` once, and taught what it
    /// ran ([`JobSizes::observe`]). A job the store no longer holds teaches nothing; a read that
    /// fails is logged and the job's run is let go (the history is a guide, not a record).
    pub(crate) fn learn_freed<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        live: &[AllocationRecord],
    ) {
        let freed = self.engine().sizes().freed(live);
        for (id, booked) in freed {
            match jobs.get_job(&id) {
                Ok(job) => {
                    let mut engine = self.engine();
                    job.iter().for_each(|job| {
                        let _ = engine.sizes().observe(job, booked);
                    });
                }
                Err(error) => {
                    warn!(job = %id, %error, "a freed job was not read; its run teaches no size")
                }
            }
        }
    }

    /// Teaches the history every ended job `jobs` holds whose allocation is freed, with what it
    /// booked, in the order the jobs ended, and takes the live allocations as those of the last pass: what a
    /// server does once at its start, before its first pass, so that the history outlives a
    /// restart for as long as the store keeps the jobs. Answers how many runs were taught.
    ///
    /// # Errors
    /// The first [`JobStoreError`] of a read: nothing is taught.
    pub fn learn_ended<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
    ) -> Result<usize, JobStoreError> {
        let mut ended = Vec::new();
        let mut resume = None;
        loop {
            let page = jobs.list_jobs(None, resume.as_ref(), MAX_PAGE)?;
            for job in page.jobs.into_iter().filter(|job| job.result.is_some()) {
                let allocation = jobs.get_allocation(&job.id)?;
                let freed =
                    allocation.filter(|allocation| allocation.step == AllocationStep::Freed);
                ended.extend(freed.map(|allocation| (job, allocation.booked)));
            }
            resume = page.resume;
            if resume.is_none() {
                break;
            }
        }
        ended.sort_by_key(|(job, _)| job.ended_at);
        let live = jobs.live_allocations()?;
        let mut engine = self.engine();
        let sizes = engine.sizes();
        let taught = ended
            .iter()
            .filter(|(job, booked)| sizes.observe(job, *booked))
            .count();
        let _ = sizes.freed(&live);
        Ok(taught)
    }
}

#[cfg(test)]
#[path = "sizes_tests.rs"]
mod tests;
