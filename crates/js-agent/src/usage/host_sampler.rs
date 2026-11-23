//! The load of a host with no control groups (a Mac): the whole host's CPU busy time and
//! non-file memory, from the host's own counters ([`HostStatistics`]; the system's on a Mac,
//! `usage/macos.rs`), sampled as a Linux host's `/proc/stat` and `/proc/meminfo` are.
//!
//! A Mac has no actions group, so the node's own figures are the host's: its CPU counter is
//! the host's busy time and its working set the host's non-file memory, with no active file
//! pages and no CPU limit. No lease has a group to read either: a lease asked for is counted
//! unreadable, and the load carries no lease minutes. The reading holds what every lease used,
//! so the collector names each lease that ran the whole window covered
//! ([`NodeSampler::covers_every_lease`]).
//!
//! The host's CPU counters are 32-bit tick counts summed over its CPUs, which wrap (at 100
//! ticks a second on 24 CPUs, in about three weeks). The sampler widens them: each reading
//! adds what each counter moved since the last one (modulo 2^32), so the busy time it reports
//! only grows. The first reading counts every tick since boot that the counters still hold.

use super::clock::UsageClock;
use super::host::HostCpu;
use super::sampler::{LeaseCgroup, NodeSampler, Sample, SampleError};

/// One reading of the host's CPU counters: the ticks each CPU state has had, summed over the
/// CPUs, each a 32-bit counter that wraps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTicks {
    /// In user mode.
    pub user: u32,
    /// In the kernel.
    pub system: u32,
    /// Idle.
    pub idle: u32,
    /// In user mode at a lowered priority.
    pub nice: u32,
}

/// The pages of a Mac's memory that are not a file's, from its VM statistics
/// (`vm_statistics64`): what Activity Monitor calls the memory used, less nothing a file holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmPages {
    /// Pages of anonymous memory (`internal_page_count`), purgeable ones among them.
    pub internal: u64,
    /// Pages of anonymous memory their owner marked purgeable (`purgeable_count`), which the
    /// kernel may drop.
    pub purgeable: u64,
    /// Pages wired down (`wire_count`): the kernel's, and what processes locked.
    pub wired: u64,
    /// Pages the compressor holds (`compressor_page_count`): the compressed anonymous memory.
    pub compressed: u64,
}

impl VmPages {
    /// The non-file memory these pages hold, in bytes, with pages of `page_bytes`: the
    /// anonymous pages that are not purgeable, the wired ones and the compressor's, saturating.
    #[must_use]
    pub fn nonfile_bytes(self, page_bytes: u64) -> u64 {
        self.internal
            .saturating_sub(self.purgeable)
            .saturating_add(self.wired)
            .saturating_add(self.compressed)
            .saturating_mul(page_bytes)
    }
}

/// The counters of a host with no control groups: the system's on a Mac, a fake in the tests.
pub trait HostStatistics {
    /// The host's CPU ticks now, or `None` when they cannot be read.
    fn cpu_ticks(&self) -> Option<CpuTicks>;

    /// How many ticks the counters of [`HostStatistics::cpu_ticks`] count a second.
    fn ticks_per_second(&self) -> u64;

    /// The host's non-file memory now, in bytes (what its processes and its kernel hold that
    /// is not a file's page it could drop), or `None` when it cannot be read.
    fn nonfile_bytes(&self) -> Option<u64>;
}

/// Samples a host by its [`HostStatistics`]. See the [module](self).
#[derive(Debug)]
pub struct HostSampler<S: HostStatistics> {
    stats: S,
    /// The last reading of the CPU counters, and the busy ticks counted up to it.
    busy: Option<(CpuTicks, u64)>,
}

impl<S: HostStatistics> HostSampler<S> {
    /// A sampler of the host `stats` reads, with no reading yet.
    pub fn new(stats: S) -> Self {
        Self { stats, busy: None }
    }

    /// The host's busy time, user, system and nice, in microseconds since the counters began
    /// (see the [module](self)); `None` when the counters cannot be read.
    fn busy_usec(&mut self) -> Option<u64> {
        let now = self.stats.cpu_ticks()?;
        let ticks = match self.busy {
            None => [now.user, now.system, now.nice]
                .into_iter()
                .map(u64::from)
                .sum(),
            Some((last, counted)) => {
                let moved = |now: u32, last: u32| u64::from(now.wrapping_sub(last));
                counted
                    .saturating_add(moved(now.user, last.user))
                    .saturating_add(moved(now.system, last.system))
                    .saturating_add(moved(now.nice, last.nice))
            }
        };
        self.busy = Some((now, ticks));
        let per_second = u128::from(self.stats.ticks_per_second().max(1));
        let usec = u128::from(ticks) * 1_000_000 / per_second;
        Some(u64::try_from(usec).unwrap_or(u64::MAX))
    }
}

impl<S: HostStatistics> NodeSampler for HostSampler<S> {
    /// The host's busy time and non-file memory as the node's, with exactly one read of
    /// `clock`, right after the CPU counters. Counters that cannot be read fail the sample,
    /// so that no zero is invented. Each of `leases` is counted unreadable: no lease of a
    /// host without control groups has a group to read.
    fn sample(
        &mut self,
        clock: &dyn UsageClock,
        leases: &[LeaseCgroup],
    ) -> Result<Sample, SampleError> {
        let busy = self.busy_usec().ok_or(SampleError::HostCpuLoad)?;
        let mono_usec = clock.now_usec();
        let nonfile = self.stats.nonfile_bytes().ok_or(SampleError::HostMemory)?;
        Ok(Sample {
            mono_usec,
            cpu_usage_usec: busy,
            host_cpu_usec: busy,
            working_set_bytes: nonfile,
            host_nonfile_bytes: Some(nonfile),
            active_file_bytes: 0,
            cpu_limited: false,
            leases: Vec::new(),
            unreadable_leases: u32::try_from(leases.len()).unwrap_or(u32::MAX),
        })
    }

    /// The host's busy time, with no CPU pressure: a Mac has no such counter.
    fn host_counters(&mut self) -> Option<HostCpu> {
        self.busy_usec().map(|busy_usec| HostCpu {
            busy_usec,
            pressure_some_usec: None,
        })
    }

    /// A reading is the whole host's, which holds what every lease used.
    fn covers_every_lease(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "host_sampler_tests.rs"]
mod tests;
