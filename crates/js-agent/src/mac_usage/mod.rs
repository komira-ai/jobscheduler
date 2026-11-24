//! What a native run on a Mac uses: its CPU and its memory, sampled from its processes at each
//! poll of the native runtime ([`RunUsage`]), and what its supervisor's wait of the job said
//! ([`JobRusage`]), as the job's result reports them: [`JobUsage`], and with `--cpu-series` and
//! `--memory-series` its series ([`crate::series`]), as a Linux run's are.
//!
//! A native run has no control group: its processes are those of the account the run is lent,
//! which nothing else runs as (the supervisor among them, once it has dropped to the account).
//! At each poll the runtime reads, through [`ProcessUsage`], each live process of the account's
//! uid (`proc_pid_rusage` on a Mac, [`macos`]):
//!
//! - **its CPU**: its user and system time and those of its children it has waited for. When
//!   a process ends and its parent waits for it, its time moves into the parent's, so the sum
//!   over the processes does not fall; it falls only when a process the account no longer
//!   parents ends (one the job left to run, reaped by `launchd`). The run's CPU is the most that
//!   sum has been: a sample never takes back CPU already counted, and what such a process used
//!   after the last sample before its end is not counted.
//! - **its memory**: its physical footprint (what the kernel counts as the process's own:
//!   anonymous, wired and compressed memory, and no file page it could drop), summed over the
//!   processes. The run's peak is the most any one sample held.
//!
//! At the end, the job's usage is the more of each from the samples and from the supervisor's
//! `wait4` of the job (the CPU of the job and of every descendant it waited for, and the
//! largest resident set one of them reached), so a run too short for any sample still has
//! both. A run with neither (no sample, and an exit with no usage: a supervisor from before
//! the record had it) reports none, never a zero.
//!
//! Nothing here reads a clock or the system: the runtime gives the times, on its monotonic
//! clock in microseconds, and the processes' usage.

use std::io;

use js_agent_wire::{CpuSeries, JobUsage, MemorySeries};

use crate::mac_supervisor::JobRusage;
use crate::series::cpu::SeriesRecorder;
use crate::series::memory::MemoryRecorder;

#[cfg(target_os = "macos")]
pub mod macos;

/// What one live process of a run has used, as one sample reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcUsage {
    /// Its CPU time, user and system, and that of its children it has waited for, in
    /// microseconds.
    pub cpu_usec: u64,
    /// Its physical footprint now, in bytes.
    pub footprint_bytes: u64,
}

/// Reads the usage of the live processes of a uid: the system's on a Mac ([`macos`]), a fake
/// in the tests.
pub trait ProcessUsage: Send + Sync {
    /// The usage of each live process whose effective or real uid is `uid`. A process that
    /// ends while it is read is left out.
    ///
    /// # Errors
    ///
    /// The processes of the uid cannot be listed: the sample is then not taken.
    fn of_uid(&self, uid: u32) -> io::Result<Vec<ProcUsage>>;
}

/// Which series the runs record (`--cpu-series`, `--memory-series`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Series {
    /// The CPU, second by second.
    pub cpu: bool,
    /// The memory, second by second.
    pub memory: bool,
}

/// The usage of one run, from the samples of its processes. Pure: the caller reads the clock
/// and the processes. See the [module](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUsage {
    /// Whether a sample was taken.
    sampled: bool,
    /// The most CPU the run's processes had used together at a sample, in microseconds.
    cpu_usec: u64,
    /// The most footprint they held together at a sample, in bytes.
    peak_bytes: u64,
    /// The CPU series, with `--cpu-series`.
    cpu_series: Option<SeriesRecorder>,
    /// The memory series, with `--memory-series`.
    memory_series: Option<MemoryRecorder>,
}

impl RunUsage {
    /// The usage of a run started at `at` microseconds, which has used nothing yet, recording
    /// the series `series` names.
    #[must_use]
    pub fn starting(at: u64, series: Series) -> Self {
        Self {
            sampled: false,
            cpu_usec: 0,
            peak_bytes: 0,
            cpu_series: series.cpu.then(|| SeriesRecorder::starting(at, 0)),
            memory_series: series.memory.then(|| MemoryRecorder::starting(at)),
        }
    }

    /// Takes the sample of the run's `processes` at `at` microseconds: their CPU and their
    /// footprint, each summed over them.
    pub fn observe(&mut self, at: u64, processes: &[ProcUsage]) {
        let cpu = processes
            .iter()
            .fold(0, |sum: u64, process| sum.saturating_add(process.cpu_usec));
        let held = processes.iter().fold(0, |sum: u64, process| {
            sum.saturating_add(process.footprint_bytes)
        });
        self.sampled = true;
        self.cpu_usec = self.cpu_usec.max(cpu);
        self.peak_bytes = self.peak_bytes.max(held);
        let used = self.cpu_usec;
        self.cpu_series
            .iter_mut()
            .for_each(|series| series.observe(at, used));
        self.memory_series
            .iter_mut()
            .for_each(|series| series.observe(at, held));
    }

    /// Records that the run was seen to end at `at` microseconds: its series end there.
    pub fn end_at(&mut self, at: u64) {
        self.cpu_series
            .iter_mut()
            .for_each(|series| series.end_at(at));
        self.memory_series
            .iter_mut()
            .for_each(|series| series.end_at(at));
    }

    /// The job's usage: for each of the CPU and the peak memory, the more of what the samples
    /// saw and what the supervisor's wait `reaped` said. `None` with neither a sample nor a
    /// wait's usage. The fields a Mac has no counter for (throttling, pressure, swap, the CPU
    /// peak) are left out.
    #[must_use]
    pub fn usage(&self, reaped: Option<JobRusage>) -> Option<JobUsage> {
        let sampled = self.sampled.then_some((self.cpu_usec, self.peak_bytes));
        let waited = reaped.map(|reaped| (reaped.cpu_usec, reaped.max_rss_bytes));
        let (cpu, peak) = [sampled, waited].into_iter().flatten().reduce(
            |(cpu, peak), (more_cpu, more_peak)| (cpu.max(more_cpu), peak.max(more_peak)),
        )?;
        Some(JobUsage {
            peak_memory_bytes: peak,
            cpu_usage_usec: cpu,
            cpu_throttle: None,
            cpu_pressure_some_usec: None,
            cpu_peak: None,
            peak_swap_bytes: None,
        })
    }

    /// The CPU series at `at` microseconds (or where the run was seen to end), its last sample
    /// the job's whole CPU ([`RunUsage::usage`] of `reaped`); `None` without `--cpu-series`, or
    /// for a run with no time.
    #[must_use]
    pub fn cpu_series(&self, at: u64, reaped: Option<JobRusage>) -> Option<CpuSeries> {
        let used = self
            .usage(reaped)
            .map_or(self.cpu_usec, |usage| usage.cpu_usage_usec);
        self.cpu_series.clone()?.finish(at, used)
    }

    /// The memory series at `at` microseconds (or where the run was seen to end); `None`
    /// without `--memory-series`, or for a run with no sample or no time.
    #[must_use]
    pub fn memory_series(&self, at: u64) -> Option<MemorySeries> {
        self.memory_series.clone()?.finish(at)
    }
}

/// `ticks` of the Mac's absolute time, which a ratio of `numer` over `denom` turns into
/// nanoseconds (`mach_timebase_info`), as microseconds, rounded down and saturating. A ratio
/// with a zero denominator (which the system never gives) counts the ticks as nanoseconds.
#[must_use]
pub fn ticks_to_usec(ticks: u64, numer: u32, denom: u32) -> u64 {
    let (numer, denom) = match denom {
        0 => (1, 1),
        denom => (u128::from(numer), u128::from(denom)),
    };
    let micros = u128::from(ticks) * numer / denom / 1_000;
    u64::try_from(micros).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
