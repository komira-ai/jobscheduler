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
//! - **Learned.** A class with at least [`js_sizing::N_MIN`] fresh runs books the estimate of
//!   [`js_sizing::Summaries::estimate`]: `1.2 x` the largest peak of its recent runs plus
//!   128 MiB (at least 1 GiB, never below a memory kill's floor).
//! - **Fallback.** A job may name a second class in its label [`SIZE_FALLBACK_LABEL`] (a build
//!   farm names the kind of step, where its `size-class` names the target). A class with fewer
//!   runs books the memory its fallback's runs ask for ([`js_sizing::Summaries::peak_memory`]:
//!   `1.2 x` their largest recent peak plus 128 MiB, whatever their number or spread): the
//!   fallback class's, else, when the job names none or that class has no fresh run, the
//!   client's whole history on the job's platform ([`JobSizes::client_key_of`], which every run
//!   of the client but a memory kill teaches). Its own runs and its kill floor raise it: it books the largest of
//!   the three, so a class is never booked below a peak of its own. The fallback class is the
//!   class of that name, under the same client and platform (`docs/design/sizing.md`, 7.8).
//! - **Cold.** A class with no history of its own, whose client has no fresh run either, books
//!   the cold size ([`ColdSize`]): the server's `cold_cpu_millis` and `cold_memory_bytes`, which
//!   default to [`DEFAULT_COLD_CPU_MILLIS`] (22 cores) and [`DEFAULT_COLD_MEMORY_BYTES`] (4 GiB);
//!   its CPU never more than a quarter of the most cores a known host has (the most any class
//!   books, so that the first run of an unknown class has room to show its peak), its memory
//!   never more than the most a known host has ([`Bounds`] of [`bounds_of`]) and at least the
//!   class's memory floor. The cold memory is also the estimator's default booking.
//! - **Too large.** A class whose booking reaches the most bookable memory a host has books
//!   all of that memory: it waits for a host with nothing else on it.
//! - **CPU.** The learned CPU of the class, else of its fallback class, else of the client's
//!   whole history (the first with a CPU sample): the 90th percentile of its runs' peaks (the
//!   most cores a run used over five seconds, as its host measured it), rounded up to a quarter
//!   core, and at most a quarter of the most cores a host has; the cold CPU when none has a
//!   sample. A booking is a share of its host's CPU, not a ceiling, and placement packs these
//!   peaks.
//!
//! [`JobSizes::sized_from`] says which of these sized a job, and each placement of a job the
//! history sized is logged with it.
//!
//! A dimension the job names is booked as named, and the history never changes it. The size is
//! decided at each pass, not stored on the job: the job's own needs stay as submitted, so a
//! retried submit with the same id is still the same job, and a queued job is booked by the
//! history as it stands when it is placed. Its allocation records what it was booked.
//!
//! The memory booking is the job's limit on its host (a contained job's `memory.max`); the
//! CPU booking is its share of the host's CPU and what placement packs with, not a ceiling. The
//! CPU slack and the memory percent of its host bound both as they bound every booking, and
//! nothing here books above them.
//!
//! # The history
//!
//! The estimator learns from a job when its allocation is freed: [`JobSizes::freed`] names
//! the jobs whose live allocations are gone since the last pass, and
//! [`JobSizes::observe`] feeds each one that ended with a result, with what its allocation
//! booked. At a server's start, [`JobSizes::observe`] is fed every ended job the job store
//! still holds, in the order they ended ([`crate::place::SchedPlacer::learn_ended`]): the job
//! records are the history, and nothing else is stored. A run is taught to the job's class
//! and, when the job names one, to its fallback class (once, when the two are one class), and,
//! unless it is a memory kill, to its client's whole history on its platform
//! ([`JobSizes::client_key_of`]).
//!
//! A run is one sample of [`js_sizing::Run`]:
//!
//! - its outcome is its result's: an exit with code 0 is a success, any other exit, a wall
//!   limit or a failed input a failure (which teaches the memory nothing), a cancel a cancel,
//!   and a stop for memory a kill at its own limit (a contained job's memory limit is its
//!   booking), which raises the class's floor to twice the booking it died at;
//! - its peak memory and CPU time are what its host reported in the job's result (`usage`, read
//!   from the job's control group); a job with none has no peak, and a success with no peak teaches
//!   the memory nothing; its wall time is from its start to its end;
//! - a success whose peak came within 5% of its booking counts as having hit its limit, so it
//!   can hold an estimate at the booking but never lower it (the estimator's own censoring
//!   rule assumes the build-host limit, half as much again above the booking);
//! - its CPU contention is its own: how long some task of its group waited for a CPU per unit
//!   of wall time (`cpu.pressure`, as its host reported it). Below a tenth, the sample is a
//!   measurement; at or above it the run's CPU may only raise its class's CPU, since a run that
//!   waited used less than it would have alone. A run whose host reported no pressure (an agent
//!   from before it, and every run recorded before it) counts as having waited for nothing, as
//!   every run did before: whether it was held is what decides;
//! - its CPU was held at a ceiling when its group was throttled in at least one period in 20,
//!   or, when its host reported no throttle counters (every run recorded under the `cpu.max`
//!   its booking used to set), when its mean CPU came within 5% of its booked CPU. A held run's
//!   CPU is a lower bound and may only raise its class's CPU: the history a server rebuilds
//!   at its start from runs taken under a ceiling therefore never holds a class down at it;
//! - its CPU sample is its peak, the most cores its group used over five seconds (`cpu_peak`,
//!   as its host reported it). A run with no peak reported (an agent from before it, and every
//!   run recorded before it) has only its mean, which is a lower bound of its peak: it counts
//!   as held.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    AllocationRecord, AllocationStep, Booked, IdSource, JobId, JobRecord, JobStore, JobStoreError,
    MAX_PAGE, Needs, ResourceUsage, ResultReason,
};
use js_sizing::{
    Basis, Bounds, Day, Hint, Instance, Key, N_MIN, Outcome, PlatformClass, Run, STALE_DAYS, Shape,
    Summaries, Summary,
};
use js_types::HostId;
use js_types::sched::FarmTime;
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};

use crate::place::SchedPlacer;

pub use js_types::{SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL};

/// The CPU a class books unless the server sets another when neither it, its fallback class nor
/// its client has a CPU sample: 22 cores, a quarter of the farm's 88-core hosts, and held at a
/// quarter of the largest host the engine knows: the most any class books, so that the first
/// run of an unknown class has the room to show its peak (`docs/design/sizing.md`, 7.9).
pub const DEFAULT_COLD_CPU_MILLIS: u64 = 22_000;

/// The memory a class books unless the server sets another when neither it, its fallback class
/// nor its client has a fresh run: 4 GiB, above the largest peak (3.2 GB) a build job of the
/// farm this was measured on reached.
pub const DEFAULT_COLD_MEMORY_BYTES: u64 = 4 << 30;

/// The least CPU a cold class books: a quarter core.
const CPU_STEP: u64 = 250;

/// The milliseconds of a day.
const DAY_MILLIS: u64 = 86_400_000;

/// A peak within this many hundredths of its booking hit the booking, and so did a mean CPU
/// within this many hundredths of the CPU a run was held to.
const AT_LIMIT_PERCENT: u64 = 95;

/// A run throttled in at least one period in this many was held at its CPU ceiling (5%).
const HELD_PERIODS: u64 = 20;

/// A class books at most this share of the most cores a host has (one quarter, 22 of 88
/// cores): no class's history books a host's CPU alone, four jobs bursting at their peaks at
/// once fit a host, and a job that waits for that much room (the walk holds a host for the
/// first job that waits on it) leaves at most a quarter of the host unbooked meanwhile.
const HOST_CPU_SHARE: u64 = 4;
/// What a class with no estimate books ([`JobSizes::sized`]): the server's `cold_cpu_millis`
/// and `cold_memory_bytes`, by default ([`ColdSize::default`]) 22 cores and 4 GiB. A CPU above a
/// quarter of the most cores a known host has books that quarter, and a memory above the most a
/// known host has books that most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdSize {
    /// The CPU a cold class books, in millicores; at least 250 (a quarter core) when the
    /// server reads it from its file or flags.
    pub cpu_millis: u64,
    /// The memory a cold class books, in bytes, and the least a class books before five runs;
    /// at least 1 GiB when the server reads it from its file or flags.
    pub memory_bytes: u64,
}

impl Default for ColdSize {
    /// [`DEFAULT_COLD_CPU_MILLIS`] and [`DEFAULT_COLD_MEMORY_BYTES`]: 22 cores and 4 GiB.
    fn default() -> Self {
        Self {
            cpu_millis: DEFAULT_COLD_CPU_MILLIS,
            memory_bytes: DEFAULT_COLD_MEMORY_BYTES,
        }
    }
}

impl ColdSize {
    /// `bounds` with this cold memory as its default booking, held at the host's memory.
    fn applied(self, bounds: Bounds) -> Bounds {
        Bounds {
            default_memory: self.memory_bytes.min(bounds.host_memory),
            ..bounds
        }
    }

    /// The CPU a cold class books under `bounds`: this cold CPU held at a quarter of the most
    /// cores a host has ([`host_share`]), and at least a quarter core.
    fn cpu(self, bounds: &Bounds) -> u64 {
        self.cpu_millis.min(host_share(bounds)).max(CPU_STEP)
    }
}

pub use js_jobs::SizedFrom;

/// The history of every class the server has seen, and the job allocations live at the last
/// pass.
#[derive(Debug, Default)]
pub struct JobSizes {
    /// What a class with no estimate books.
    cold: ColdSize,
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
    /// No history, a class with no estimate booking the default cold size: 22 cores and 4 GiB.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// No history, a class with no estimate booking `cold`.
    #[must_use]
    pub fn with_cold(cold: ColdSize) -> Self {
        Self {
            cold,
            ..Self::default()
        }
    }

    /// The estimator's key of `job`'s class.
    #[must_use]
    pub fn key_of(job: &JobRecord) -> Key {
        key_under(job, class_hash(job))
    }

    /// The estimator's key of `job`'s fallback class, the class named by its label
    /// [`SIZE_FALLBACK_LABEL`] (keyed as a [`SIZE_CLASS_LABEL`] of that name is, under `job`'s
    /// client and platform); `None` when it names none, or names its own class.
    #[must_use]
    pub fn fallback_key_of(job: &JobRecord) -> Option<Key> {
        let fallback = job.spec.labels.get(SIZE_FALLBACK_LABEL)?;
        let key = key_under(job, label_hash(fallback));
        (key != Self::key_of(job)).then_some(key)
    }

    /// The estimator's key of the whole history of `job`'s client on `job`'s platform: every
    /// run of the client but a memory kill is taught to it (a kill raises the floor of its
    /// class and its fallback class, not of every class of the client), and a class with too
    /// few runs whose fallback class has none books from it.
    #[must_use]
    pub fn client_key_of(job: &JobRecord) -> Key {
        key_under(job, text_hash("client"))
    }

    /// The bounds the next sizes and runs are taken against, when `bounds` knows a host (else
    /// the last ones stay), with the cold memory as their default booking; the runs observed
    /// before any bounds were known are taught now.
    pub fn set_bounds(&mut self, bounds: Option<Bounds>) {
        let Some(bounds) = bounds else {
            return;
        };
        let bounds = self.cold.applied(bounds);
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
        let (sized_cpu, sized_memory, _) = self.size_of(job, day, bounds);
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

    /// What sized `job` on `day` when it leaves its CPU or its memory at zero and a host is
    /// known (the module docs); `None` for any other job, which [`JobSizes::sized`] leaves as
    /// it is.
    #[must_use]
    pub fn sized_from(&self, job: &JobRecord, day: Day) -> Option<SizedFrom> {
        let (Needs::Shape(resources), Some(bounds)) = (&job.spec.needs, &self.bounds) else {
            return None;
        };
        let named = (resources.cpu_millis() != 0) & (resources.memory_bytes() != 0);
        (!named).then(|| self.size_of(job, day, bounds).2)
    }

    /// The CPU and memory the history of `job`'s class books on `day`, under `bounds`, and
    /// which history that was (the module docs).
    ///
    /// The memory: a class with at least [`N_MIN`] fresh runs (a class idle over
    /// [`STALE_DAYS`] has none) books its own estimate. Any other class books the largest of
    /// what its fallback's runs ask for (the fallback class's, else the client's whole
    /// history's; the cold memory when the client has none), what its own runs ask for and its
    /// kill floor. A booking that reaches the most bookable memory a host has books all of it.
    fn size_of(&self, job: &JobRecord, day: Day, bounds: &Bounds) -> (u64, u64, SizedFrom) {
        let key = Self::key_of(job);
        let cpu = self.cpu_of(job, day, bounds);
        let fresh = |own: &&Summary| day.0.saturating_sub(own.last_seen().0) <= STALE_DAYS;
        let own = self.summaries.get(&key).filter(fresh);
        if own.is_some_and(|own| own.samples() >= N_MIN) {
            return match self.summaries.estimate(&key, day, bounds, &Hint::default()) {
                Ok(estimate) => (cpu, estimate.memory_bytes, SizedFrom::Class),
                // A fresh class with five runs has an estimate unless it reaches the host.
                Err(_) => (cpu, bounds.host_memory, SizedFrom::OverCap),
            };
        }
        let peak = |key: &Key| {
            let peak = self.summaries.peak_memory(key, day, bounds);
            peak.filter(|memory| *memory < bounds.host_memory)
        };
        let fallback = Self::fallback_key_of(job)
            .and_then(|key| peak(&key))
            .or_else(|| peak(&Self::client_key_of(job)));
        let (base, from) = match fallback {
            Some(memory) => (memory, SizedFrom::Fallback),
            None => (bounds.default_memory, SizedFrom::Cold),
        };
        let own_peak = self.summaries.peak_memory(&key, day, bounds).unwrap_or(0);
        let floor = self.summaries.key_floor(&key, bounds);
        // The base and the floor stay within the host; only the class's own runs reach it.
        let memory = base.max(own_peak).max(floor);
        if own_peak >= bounds.host_memory {
            (cpu, bounds.host_memory, SizedFrom::OverCap)
        } else if own_peak > base {
            (cpu, memory, SizedFrom::Class)
        } else {
            (cpu, memory, from)
        }
    }

    /// The CPU `job` books on `day` under `bounds`: the learned CPU of its own class, else of
    /// its fallback class, else of its client's whole history (the first that has a CPU
    /// sample), at most a [`HOST_CPU_SHARE`]th of the most cores a host has ([`host_share`]) and
    /// at least a quarter core; the cold CPU, held at the same share, when none has a sample.
    fn cpu_of(&self, job: &JobRecord, day: Day, bounds: &Bounds) -> u64 {
        let keys = [
            Some(Self::key_of(job)),
            Self::fallback_key_of(job),
            Some(Self::client_key_of(job)),
        ];
        let learned = keys
            .iter()
            .flatten()
            .find_map(|key| self.summaries.cpu(key, day, bounds));
        learned.map_or_else(
            || self.cold.cpu(bounds),
            |cpu| u64::from(cpu).min(host_share(bounds)).max(CPU_STEP),
        )
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

    /// Teaches the history of `job`'s class, and of its fallback class when it names one, the
    /// run of `job`, which ended booked as `booked`, on the day `job` ended (held until bounds
    /// are known). A job that has not ended, ended with no result (lost, failed to start),
    /// booked the whole host or never started teaches nothing. Answers whether the run taught
    /// the class a memory sample: a run the estimator discards (a success with no measured
    /// peak or under a second of wall time, a failure, a cancel, a success at or above the
    /// largest host's memory) is still recorded as seen, but answers `false`. Before any bounds
    /// are known the run is judged against a host of unlimited memory, so one that would
    /// reach the largest host's memory is counted until the bounds say otherwise.
    pub fn observe(&mut self, job: &JobRecord, booked: Booked) -> bool {
        let Some((run, day)) = run_of(job, booked) else {
            return false;
        };
        let taught = self.teaches(job, &run, day);
        // A kill raises the floor of the class and of its fallback, never of every class of
        // the client.
        if run.outcome != Outcome::OwnLimitKill {
            self.pending.push((Self::client_key_of(job), run, day));
        }
        if let Some(fallback) = Self::fallback_key_of(job) {
            self.pending.push((fallback, run, day));
        }
        self.pending.push((Self::key_of(job), run, day));
        self.set_bounds(self.bounds);
        taught
    }

    /// Whether the estimator would take `run` of `job` as a memory sample, judged on a summary
    /// of its own that is then dropped, against the known bounds (a host of unlimited memory
    /// before any are known).
    fn teaches(&self, job: &JobRecord, run: &Run, day: Day) -> bool {
        let bounds = self.bounds.unwrap_or(Bounds {
            default_memory: DEFAULT_COLD_MEMORY_BYTES,
            host_memory: u64::MAX,
            node_cpu_millis: u32::MAX,
        });
        let mut trial = Summary::new(Self::key_of(job).basis(), day);
        trial.observe(run, day, &bounds).memory.is_ok()
    }
}

/// The run of `job`, booked as `booked`, as the estimator takes it (the module docs), with the
/// day it ended; `None` for a job that teaches nothing.
fn run_of(job: &JobRecord, booked: Booked) -> Option<(Run, Day)> {
    let Booked::Shape {
        cpu_millis,
        memory_bytes,
        ..
    } = booked
    else {
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
    let wall = micros(ended.since(started));
    // At the limit: a peak within 5% of a booking (a booking of 0 has no limit).
    let at_limit = peak.is_some_and(|peak| {
        let peak = u128::from(peak) * 100;
        (memory_bytes > 0) & (peak >= u128::from(memory_bytes) * u128::from(AT_LIMIT_PERCENT))
    });
    let run = Run {
        outcome,
        wall_us: wall,
        peak_memory_bytes: peak,
        booked_memory_bytes: memory_bytes,
        raised_memory_bytes: killed.then(|| memory_bytes.saturating_mul(2)),
        limit_hits: u64::from(at_limit),
        swap_peak_bytes: 0,
        cpu_usage_us: cpu,
        node_cpu_pressure_permille: result.usage.map(|usage| contention(usage, wall)),
        // A run with no peak measured has only its mean, a lower bound of its peak.
        cpu_held: result
            .usage
            .is_some_and(|usage| usage.cpu_peak.is_none() | held(usage, (cpu, wall), cpu_millis)),
        cpu_peak_millis: result
            .usage
            .and_then(|usage| usage.cpu_peak)
            .map(|peak| u32::try_from(peak.millicores).unwrap_or(u32::MAX)),
    };
    Some((run, day_of_millis(millis(ended.as_duration()))))
}

/// The most CPU a class books under `bounds`: a [`HOST_CPU_SHARE`]th of the most cores a host
/// has, rounded down to a quarter core.
fn host_share(bounds: &Bounds) -> u64 {
    u64::from(bounds.node_cpu_millis) / HOST_CPU_SHARE / CPU_STEP * CPU_STEP
}

/// The run's own CPU contention, in thousandths of its wall time: how long some task of its
/// group waited for a CPU (the `some` total of its `cpu.pressure`) per unit of wall time,
/// saturating. 0 when the agent did not report the pressure (an agent from before it, and
/// every run a server before it recorded), as every run counted before it: whether such a run
/// was held at a ceiling is what [`held`] decides.
fn contention(usage: ResourceUsage, wall_us: u64) -> u16 {
    let waited = usage
        .cpu_pressure_some
        .map_or(0, |waited| u128::from(micros(waited)));
    let permille = waited * 1000 / u128::from(wall_us).max(1);
    u16::try_from(permille).unwrap_or(u16::MAX)
}

/// Whether the run's CPU was held at a ceiling, so that what it used is only a lower bound of
/// what it would have used: with the throttle counters, it was throttled in at least one
/// period in [`HELD_PERIODS`] (a group with no `cpu.max` counts no periods and is never held);
/// without them (an agent from before them, and every run a server before them recorded, which
/// ran under a `cpu.max` of its booking), its mean CPU over `(cpu_us, wall_us)` came within
/// [`AT_LIMIT_PERCENT`] of the `booked_cpu` millicores.
fn held(usage: ResourceUsage, (cpu_us, wall_us): (u64, u64), booked_cpu: u64) -> bool {
    match usage.cpu_throttle {
        Some(throttle) => {
            let throttled = u128::from(throttle.throttled_periods) * u128::from(HELD_PERIODS);
            (throttle.periods > 0) & (throttled >= u128::from(throttle.periods))
        }
        None => {
            let used = u128::from(cpu_us) * 1000 * 100;
            let cap = u128::from(booked_cpu) * u128::from(wall_us) * u128::from(AT_LIMIT_PERCENT);
            (booked_cpu > 0) & (used >= cap)
        }
    }
}

/// The bounds of hosts of `capacities` (each the CPU and the bookable memory a host offers):
/// the most memory and the most cores a host has (each on its own), with a cold booking of
/// [`DEFAULT_COLD_MEMORY_BYTES`] held at that memory ([`JobSizes::set_bounds`] puts the
/// server's cold memory in its place). `None` with no host that offers memory.
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
    (memory > 0).then(|| Bounds {
        default_memory: DEFAULT_COLD_MEMORY_BYTES.min(memory),
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

/// The estimator's key of the class whose hash is `hash`, under `job`'s client and platform,
/// on the cgroup basis.
fn key_under(job: &JobRecord, hash: u64) -> Key {
    let client = job.client.as_ref().map_or("", |client| client.as_str());
    let constraints = &job.spec.constraints;
    let os = constraints.os.as_deref().unwrap_or("");
    let arch = constraints.arch.as_deref().unwrap_or("");
    Key::new(
        Instance::new(client),
        PlatformClass::new(format!("{os}/{arch}")),
        Basis::Cgroup,
        Shape(hash),
        None,
    )
}

/// The first 8 bytes, big-endian, of the SHA-256 of `job`'s class text: `label`, NUL and its
/// `size-class` label, else `run`, NUL, its image name, NUL and its command joined by NUL.
fn class_hash(job: &JobRecord) -> u64 {
    let spec = &job.spec;
    match spec.labels.get(SIZE_CLASS_LABEL) {
        Some(class) => label_hash(class),
        None => {
            let image = spec.image.as_ref().map_or("", |image| image.name());
            text_hash(&format!("run\0{image}\0{}", spec.command.join("\0")))
        }
    }
}

/// The hash of the class named `class` by a label: that of `label`, NUL and `class`.
fn label_hash(class: &str) -> u64 {
    text_hash(&format!("label\0{class}"))
}

/// The first 8 bytes, big-endian, of the SHA-256 of `text`.
fn text_hash(text: &str) -> u64 {
    let digest = Sha256::digest(text.as_bytes());
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
    /// Logs the placement of `job` on `host` at `now` with what sized it ([`SizedFrom`]), when
    /// the history sized it: the record of which history books the farm's jobs.
    pub(crate) fn log_sized(&self, job: &JobRecord, host: &HostId, now: FarmTime) {
        let from = self.engine().sizes().sized_from(job, day_of(now));
        if let Some(from) = from {
            let class = class_name(job);
            let sized_from = from.as_str();
            info!(job = %job.id, %host, %class, sized_from, "a job the history sized was placed");
        }
    }

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
    /// restart for as long as the store keeps the jobs. Answers how many runs taught a class a
    /// memory sample ([`JobSizes::observe`]): a job that ended with no measured usage is read
    /// and not counted.
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
