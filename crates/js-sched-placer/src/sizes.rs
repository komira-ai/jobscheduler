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
//! - **Learned.** A class with a fresh memory sample of its own books what its own runs ask
//!   for ([`js_sizing::Summaries::peak_memory`]: `1.2 x` the largest peak of its recent runs
//!   plus 128 MiB, at least 1 GiB, never below a memory kill's floor), from its first run on,
//!   whatever their number or spread: neither its kind's largest peak nor the cold size holds
//!   it above what it uses.
//! - **Fallback.** A job may name a second class in its label [`SIZE_FALLBACK_LABEL`] (a build
//!   farm names the kind of step, where its `size-class` names the target). A class with no
//!   run of its own books the memory its fallback's runs ask for (the same figure of theirs):
//!   the fallback class's, else, when the job names none or that class has no fresh run, the
//!   client's whole history on the job's platform ([`JobSizes::client_key_of`], which every run
//!   of the client but a memory kill teaches), and at least its own kill floor. The fallback
//!   class is the class of that name, under the same client and platform
//!   (`docs/design/sizing.md`, 7.8).
//! - **Cold.** A class with no history of its own, whose client has no fresh run either, books
//!   the cold size ([`ColdSize`]): the server's `cold_cpu_millis` and `cold_memory_bytes`, which
//!   default to [`DEFAULT_COLD_CPU_MILLIS`] (22 cores) and [`DEFAULT_COLD_MEMORY_BYTES`] (4 GiB),
//!   and for a job whose `os` the server names in `cold_memory_bytes_on` ([`ColdMemoryOn`]) or
//!   `cold_cpu_millis_on` ([`ColdCpuOn`]) that operating system's memory or CPU in place of the
//!   server's (a Mac's jobs need not book what a build host's do); its CPU never more than a
//!   quarter of the most cores a known host has (the most any class books, so that the first run
//!   of an unknown class has room for the cores it uses), its memory never more than the most a
//!   known host has ([`Bounds`] of [`bounds_of`]) and at least the class's memory floor.
//! - **Too large.** A class whose own runs ask for the most bookable memory a host has or more
//!   books all of that memory: it waits for a host with nothing else on it.
//! - **CPU.** The learned CPU of the class, else of its fallback class, else of the client's
//!   whole history (the first with a CPU sample): the median of its runs' mean CPU (CPU time
//!   over wall time; the nearest-rank 50th percentile), rounded up to a quarter core, and at
//!   most a quarter of the most cores a host has; the cold CPU when none has a sample. A booking is a share of its
//!   host's CPU, not a ceiling. Placement packs these bookings within the CPU slack and admits
//!   by the host's measured load, so a job's bursts above its mean are the load's to judge,
//!   not its booking's, unless they are a heavy tail: a class whose runs' five-second peaks
//!   (the 90th percentile of them, over at least five runs that reported one) are over twice
//!   its median books that percentile instead, under the same quarter-host cap
//!   (`docs/design/sizing.md`, 7.10), so a burst of its jobs spreads over the hosts.
//!
//! [`JobSizes::sized_from`] says which of these sized a job, and each placement of a job the
//! history sized is logged with it.
//!
//! A dimension the job names is booked as named, and the history never changes it. The size is
//! decided at each pass, not stored on the job: the job's own needs stay as submitted, so a
//! retried submit with the same id is still the same job, and a queued job is booked by the
//! history as it stands when it is placed. Its allocation records what it was booked.
//!
//! The memory booking is what placement counts, not a limit (a contained job has no
//! `memory.max`): a job that needs more runs on what its host has, and its run raises its
//! class's next booking. The CPU booking is its share of the host's CPU and what placement packs
//! with, not a ceiling. The CPU slack and the memory percent of its host bound both as they bound
//! every booking, and nothing here books above them.
//!
//! # The history
//!
//! The estimator learns from a job when its allocation is freed: [`JobSizes::freed`] names
//! the jobs whose live allocations are gone since the last pass, and
//! [`JobSizes::observe`] feeds each one that ended with a result, with what its allocation
//! booked. After a server's start, [`JobSizes::observe`] is fed every ended job the job store
//! still holds, in the order they ended, and then the runs the passes freed meanwhile, which
//! were held for it ([`Teaching`]): the job records are the history, and nothing else is
//! stored. A run is taught to the job's class
//! and, when the job names one, to its fallback class (once, when the two are one class), and,
//! unless it is a memory kill, to its client's whole history on its platform
//! ([`JobSizes::client_key_of`]).
//!
//! A run is one sample of [`js_sizing::Run`]:
//!
//! - its outcome is its result's: an exit with code 0 is a success, any other exit, a wall
//!   limit or a failed input a failure (which teaches the memory nothing), a cancel a cancel,
//!   and a stop for memory (an agent from before the booking stopped being a limit, which held
//!   a job at its booking) a kill at its booking, which raises the class's floor to twice the
//!   booking it died at;
//! - its peak memory is the peak of the non-file memory series its host reported in the job's
//!   result (`memory_series`: what the job held that the kernel cannot drop, sampled about once
//!   a second, so no page cache of the files it read), and, for a run with no whole series, the
//!   group's `memory.peak` of its `usage`; its CPU time is its `usage`'s; a job with neither has
//!   no peak, and a success with no peak teaches the memory nothing; its wall time is from its
//!   start to its end;
//! - a success's peak is a measurement whatever its booking: the booking is no limit, so a peak
//!   above it is what the run used, and raises the class's next booking at once (with the
//!   margin);
//! - but a success whose group's `memory.peak` came within 5% of its booking and stayed within
//!   it reached its booking: a host that holds a job at its booking (the docker and podman
//!   runtime's `--memory`, whose limit counts the page cache the non-file peak leaves out)
//!   held it there, so its peak does not show what it needed. The estimator books its class a
//!   step above the booking next (`1.2 x` the booking plus 128 MiB, rounded up to 256 MiB), and
//!   again while its runs keep reaching it, up to the whole host
//!   ([`js_sizing::Run::limit_hits`]);
//! - a run under a second gives its peak (its series' when a poll sampled it, else the group's
//!   `memory.peak`, which is exact) and no CPU sample;
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
//! - its CPU sample is its mean, its CPU time over its wall time. The most cores its group used
//!   over five seconds (`cpu_peak`, as its host reported it) goes beside the sample, and books
//!   its class only when the class's peaks are a heavy tail: a peak booking of every class held
//!   a host to a few jobs whose peaks fill it while their use left most of it idle.
//!
//! # Profiles
//!
//! With [`Curves::Shadow`] each class also learns a CPU and a memory profile of its runs' recorded
//! series ([`js_sizing::Profile`]), and a run freed while serving logs how its class's profile
//! predicted it. The profiles are read by nothing that books a job.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_jobs::{
    AllocationRecord, Booked, IdSource, JobId, JobRecord, JobStore, Needs, ResourceUsage,
    ResultReason,
};
use js_sizing::{
    Basis, Bounds, Day, Instance, Key, MIN_WALL_US, Outcome, PlatformClass, Run, Shape, Summaries,
    Summary,
};
use js_types::HostId;
use js_types::sched::FarmTime;
use sha2::{Digest as _, Sha256};
use tracing::{info, warn};

use crate::place::SchedPlacer;

pub use js_types::{SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL};
pub use profile::Curves;
pub use teach::Teaching;

#[path = "sizes_profile.rs"]
mod profile;
#[path = "sizes_teach.rs"]
mod teach;

/// The CPU a class books unless the server sets another when neither it, its fallback class nor
/// its client has a CPU sample: 22 cores, a quarter of the farm's 88-core hosts, and held at a
/// quarter of the largest host the engine knows: the most any class books, so that the first
/// run of an unknown class has the room for the cores it uses (`docs/design/sizing.md`, 7.9).
pub const DEFAULT_COLD_CPU_MILLIS: u64 = 22_000;

/// The memory a class books unless the server sets another when neither it, its fallback class
/// nor its client has a fresh run: 4 GiB, above the largest peak (3.2 GB) a build job of the
/// farm this was measured on reached.
pub const DEFAULT_COLD_MEMORY_BYTES: u64 = 4 << 30;

/// The least CPU a cold class books: a quarter core.
const CPU_STEP: u64 = 250;

/// The milliseconds of a day.
const DAY_MILLIS: u64 = 86_400_000;

/// A mean CPU within this many hundredths of the CPU a run was held to was held at it, and
/// a group's memory peak within this many hundredths of its booking reached it.
const AT_LIMIT_PERCENT: u64 = 95;

/// A run throttled in at least one period in this many was held at its CPU ceiling (5%).
const HELD_PERIODS: u64 = 20;

/// A class books at most this share of the most cores a host has (one quarter, 22 of 88
/// cores): no class's history books a host's CPU alone, and a job that waits for that much
/// room (the walk holds a host for the first job that waits on it) leaves at most a quarter of
/// the host unbooked meanwhile.
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
    /// The memory a cold class books, in bytes (a class, its fallback and its client with no
    /// fresh run); at least 1 GiB when the server reads it from its file or flags.
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
}

/// `cpu_millis`, a CPU a class books (learned or cold), held at a quarter of the most cores a
/// host has under `bounds` ([`host_share`]), and at least a quarter core.
fn held_cpu(cpu_millis: u64, bounds: &Bounds) -> u64 {
    cpu_millis.min(host_share(bounds)).max(CPU_STEP)
}

/// The cold memory the server sets for the jobs of each operating system it names
/// (`--cold-memory-bytes-on`, `server.cold_memory_bytes_on`), in bytes, in place of
/// [`ColdSize::memory_bytes`]: keyed by the `os` a job's constraints name, as it names it (such
/// as `macos`). A job that names no `os`, or one with no entry, books the server's cold memory.
/// Each is held at the most bookable memory a known host has, as the server's is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColdMemoryOn(BTreeMap<String, u64>);

impl ColdMemoryOn {
    /// The cold memory of each operating system `entries` names, in bytes.
    #[must_use]
    pub fn new(entries: BTreeMap<String, u64>) -> Self {
        Self(entries)
    }

    /// The cold memory set for the jobs of `os`, if any.
    #[must_use]
    pub fn of(&self, os: &str) -> Option<u64> {
        self.0.get(os).copied()
    }
}

/// The cold CPU the server sets for the jobs of each operating system it names
/// (`--cold-cpu-millis-on`, `server.cold_cpu_millis_on`), in millicores, in place of
/// [`ColdSize::cpu_millis`]: keyed by the `os` a job's constraints name, as it names it (such
/// as `macos`). A job that names no `os`, or one with no entry, books the server's cold CPU.
/// Each is held at a quarter of the most cores a known host has, and at least a quarter core,
/// as the server's is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColdCpuOn(BTreeMap<String, u64>);

impl ColdCpuOn {
    /// The cold CPU of each operating system `entries` names, in millicores.
    #[must_use]
    pub fn new(entries: BTreeMap<String, u64>) -> Self {
        Self(entries)
    }

    /// The cold CPU set for the jobs of `os`, if any.
    #[must_use]
    pub fn of(&self, os: &str) -> Option<u64> {
        self.0.get(os).copied()
    }
}

pub use js_jobs::SizedFrom;

/// The history of every class the server has seen, and the job allocations live at the last
/// pass.
#[derive(Debug, Default)]
pub struct JobSizes {
    /// What a class with no estimate books.
    cold: ColdSize,
    /// The cold memory of the jobs of an operating system the server names.
    cold_on: ColdMemoryOn,
    /// The cold CPU of the jobs of an operating system the server names.
    cold_cpu_on: ColdCpuOn,
    summaries: Summaries,
    /// Each job whose allocation was live at the last pass, with what it booked.
    live: BTreeMap<JobId, Booked>,
    /// The bounds of the hosts the engine knows, from the last walk that knew one: what the
    /// sizes and the runs are taken against. `None` before any.
    bounds: Option<Bounds>,
    /// The runs observed before any bounds were known, in the order they came: taught when
    /// the first bounds come.
    pending: Vec<(Key, Run, Day)>,
    /// What a teaching of the ended jobs begun and not taught ([`Teaching`]) holds; `None`
    /// otherwise, when a freed job is taught at once.
    held: Option<Held>,
    /// The CPU and memory profiles learned in shadow, a pair for each class; `None` with
    /// [`Curves::Off`]. Nothing the sizes book reads them.
    profiles: Option<profile::Profiles>,
}

/// The freed jobs a teaching of the ended jobs holds until it is taught ([`Teaching`]).
#[derive(Debug, Default)]
struct Held {
    /// Each job a pass freed, with what it booked, in the order the passes freed them: taught
    /// after the ended jobs.
    runs: Vec<(JobRecord, Booked)>,
    /// Every job [`JobSizes::freed`] named since the teaching began, its run held or not yet:
    /// the passes teach them, so the teaching leaves them out of the ended jobs it read.
    named: BTreeSet<JobId>,
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

    /// The same sizes, a class with no estimate of a job whose `os` is one `on` names booking
    /// that operating system's cold memory in place of the server's.
    #[must_use]
    pub fn with_cold_memory_on(self, on: ColdMemoryOn) -> Self {
        Self {
            cold_on: on,
            ..self
        }
    }

    /// The same sizes, a class with no CPU sample of a job whose `os` is one `on` names booking
    /// that operating system's cold CPU in place of the server's.
    #[must_use]
    pub fn with_cold_cpu_on(self, on: ColdCpuOn) -> Self {
        Self {
            cold_cpu_on: on,
            ..self
        }
    }

    /// This history with the profiles `curves` names: [`Curves::Off`] keeps none,
    /// [`Curves::Shadow`] and [`Curves::On`] learn a CPU and a memory profile for each class
    /// from every run they observe, and log a line for each run freed while serving. The sizes
    /// it books ([`JobSizes::sized`]) are the same in all three.
    #[must_use]
    pub fn with_curves(mut self, curves: Curves) -> Self {
        self.profiles = (curves != Curves::Off).then(profile::Profiles::default);
        self
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
    /// The memory: a class with a fresh memory sample of its own (a class idle over
    /// [`js_sizing::STALE_DAYS`] has none) books what its own runs ask for
    /// ([`js_sizing::Summaries::peak_memory`]: `1.2 x` their largest recent peak plus 128 MiB,
    /// at least 1 GiB, and its kill floor), from its first run on, whatever their number or
    /// spread. Any other class books what its fallback's runs ask for (the fallback class's,
    /// else the client's whole history's; the cold memory when the client has none), and its
    /// kill floor. A booking that reaches the most bookable memory a host has books all of it.
    fn size_of(&self, job: &JobRecord, day: Day, bounds: &Bounds) -> (u64, u64, SizedFrom) {
        let key = Self::key_of(job);
        let cpu = self.cpu_of(job, day, bounds);
        let floor = self.summaries.key_floor(&key, bounds);
        if let Some(own) = self.summaries.peak_memory(&key, day, bounds) {
            return if own >= bounds.host_memory {
                (cpu, bounds.host_memory, SizedFrom::OverCap)
            } else {
                (cpu, own.max(floor), SizedFrom::Class)
            };
        }
        let peak = |key: &Key| {
            let peak = self.summaries.peak_memory(key, day, bounds);
            peak.filter(|memory| *memory < bounds.host_memory)
        };
        let fallback = Self::fallback_key_of(job)
            .and_then(|key| peak(&key))
            .or_else(|| peak(&Self::client_key_of(job)));
        // The base and the floor stay within the host.
        match fallback {
            Some(memory) => (cpu, memory.max(floor), SizedFrom::Fallback),
            None => (
                cpu,
                self.cold_memory(job, bounds).max(floor),
                SizedFrom::Cold,
            ),
        }
    }

    /// The cold memory `job` books under `bounds`: the one set for its `os` ([`ColdMemoryOn`])
    /// held at the most bookable memory a host has, else the server's (held there already).
    fn cold_memory(&self, job: &JobRecord, bounds: &Bounds) -> u64 {
        let os = job.spec.constraints.os.as_deref().unwrap_or("");
        self.cold_on.of(os).map_or(bounds.default_memory, |memory| {
            memory.min(bounds.host_memory)
        })
    }

    /// The CPU `job` books on `day` under `bounds`: the learned CPU of its own class, else of
    /// its fallback class, else of its client's whole history (the first that has a CPU
    /// sample), at most a [`HOST_CPU_SHARE`]th of the most cores a host has ([`host_share`]) and
    /// at least a quarter core; the cold CPU ([`JobSizes::cold_cpu`]), held at the same share,
    /// when none has a sample.
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
            || self.cold_cpu(job, bounds),
            |cpu| held_cpu(u64::from(cpu), bounds),
        )
    }

    /// The cold CPU `job` books under `bounds`: the one set for its `os` ([`ColdCpuOn`]), else
    /// the server's, held at a quarter of the most cores a host has and at least a quarter core.
    fn cold_cpu(&self, job: &JobRecord, bounds: &Bounds) -> u64 {
        let os = job.spec.constraints.os.as_deref().unwrap_or("");
        let cpu = self.cold_cpu_on.of(os).unwrap_or(self.cold.cpu_millis);
        held_cpu(cpu, bounds)
    }

    /// The jobs whose allocations were live at the last call and are not in `live` now, each
    /// with what it booked; `live` is kept for the next call.
    pub fn freed(&mut self, live: &[AllocationRecord]) -> Vec<(JobId, Booked)> {
        let now: BTreeMap<JobId, Booked> = live
            .iter()
            .map(|allocation| (allocation.job.clone(), allocation.booked))
            .collect();
        let before = std::mem::replace(&mut self.live, now);
        let freed: Vec<(JobId, Booked)> = before
            .into_iter()
            .filter(|(job, _)| !self.live.contains_key(job))
            .collect();
        if let Some(held) = &mut self.held {
            held.named.extend(freed.iter().map(|(job, _)| job.clone()));
        }
        freed
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
    /// peak, a failure, a cancel, a success at or above the
    /// largest host's memory) is still recorded as seen, but answers `false`. Before any bounds
    /// are known the run is judged against a host of unlimited memory, so one that would
    /// reach the largest host's memory is counted until the bounds say otherwise.
    pub fn observe(&mut self, job: &JobRecord, booked: Booked) -> bool {
        self.observe_logged(job, booked, false)
    }

    /// [`JobSizes::observe`], which also logs the check of the run against its class's profile
    /// when `log` is set (a run freed while serving, not one read from the history).
    fn observe_logged(&mut self, job: &JobRecord, booked: Booked, log: bool) -> bool {
        let Some((run, day)) = run_of(job, booked) else {
            return false;
        };
        self.profile_run(job, &run, log);
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

    /// Teaches the run of `job`, freed booked as `booked` ([`JobSizes::observe`]), or holds it
    /// while a teaching of the ended jobs is begun and not taught ([`Teaching`]).
    fn freed_run(&mut self, job: JobRecord, booked: Booked) {
        match &mut self.held {
            Some(held) => held.runs.push((job, booked)),
            None => {
                let _ = self.observe_logged(&job, booked, true);
            }
        }
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

/// The memory a run of `result` teaches its class: the peak of its non-file memory series (the
/// most the job held that the kernel cannot drop, sampled about once a second) when its host
/// reported a whole one with a point; else the group's `memory.peak`, which counts the page
/// cache of the files the job read too (an agent without `--memory-series`, a runtime that
/// records no series, a run adopted after a restart of its agent, whose series misses its
/// start, and a run so short no poll sampled it).
fn peak_of(result: &js_jobs::JobResult) -> Option<u64> {
    let whole = result
        .memory_series
        .as_ref()
        .filter(|series| !series.partial);
    whole
        .and_then(js_types::MemorySeries::peak_bytes)
        .or_else(|| result.usage.map(|usage| usage.peak_memory_bytes))
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
    let peak = peak_of(result);
    let cpu = result.usage.map_or(0, |usage| micros(usage.cpu_time));
    let wall = micros(ended.since(started));
    // A successful run under a second gives its peak (its series' if a poll sampled it, else
    // the group's `memory.peak`, exact whatever the run's length) and no CPU sample (its mean
    // over so short a wall is no measure): the estimator takes a memory sample from a run of
    // at least a second, and a CPU sample only from a run that used CPU time. No other outcome
    // is a sample of either.
    let (wall_us, cpu_us) = if wall < MIN_WALL_US {
        (MIN_WALL_US, 0)
    } else {
        (wall, cpu)
    };
    let run = Run {
        outcome,
        wall_us,
        peak_memory_bytes: peak,
        // A run's peak is a measurement whatever its booking, unless the run reached its
        // booking (below); a kill keeps its booking, which its floor is raised from.
        booked_memory_bytes: memory_bytes,
        raised_memory_bytes: killed.then(|| memory_bytes.saturating_mul(2)),
        limit_hits: u64::from(
            result
                .usage
                .is_some_and(|usage| reached(usage, memory_bytes)),
        ),
        swap_peak_bytes: 0,
        cpu_usage_us: cpu_us,
        node_cpu_pressure_permille: result.usage.map(|usage| contention(usage, wall)),
        cpu_held: result
            .usage
            .is_some_and(|usage| held(usage, (cpu, wall), cpu_millis)),
        // The sample is the run's mean CPU; its five-second peak goes beside it, and books
        // its class only when the class's peaks are a heavy tail far above its median.
        cpu_peak_millis: result
            .usage
            .and_then(|usage| usage.cpu_peak)
            .map(|peak| u32::try_from(peak.millicores).unwrap_or(u32::MAX)),
    };
    Some((run, day_of_millis(millis(ended.as_duration()))))
}

/// Whether a run booked `booked` bytes reached its booking: its group's `memory.peak` came
/// within [`AT_LIMIT_PERCENT`] of the booking and stayed within it, as a host that holds a job
/// at its booking leaves it. A booking of 0 is never reached, and a group peak past the
/// booking is a run that used more than it booked, which no host held: a measurement.
fn reached(usage: ResourceUsage, booked: u64) -> bool {
    let peak = u128::from(usage.peak_memory_bytes);
    let booked = u128::from(booked);
    (booked > 0) & (peak <= booked) & (peak * 100 >= booked * u128::from(AT_LIMIT_PERCENT))
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
    /// ran ([`JobSizes::observe`]), or held until the teaching of the ended jobs begun is taught
    /// ([`Teaching`]). A job the store no longer holds teaches nothing; a read that
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
                    job.into_iter()
                        .for_each(|job| engine.sizes().freed_run(job, booked));
                }
                Err(error) => {
                    warn!(job = %id, %error, "a freed job was not read; its run teaches no size")
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "sizes_tests.rs"]
mod tests;
