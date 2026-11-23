//! The whole host's CPU ([`host_cpu`]): the busy time of every CPU, read by the heartbeat
//! load's own reader of `/proc/stat` ([`parse_host_busy_usec`]), and the `some` total of
//! `/proc/pressure/cpu`, read through a [`FileReader`]. The busy time is the same counter the
//! load's `host_cpu_usec` carries, so the per-second series and the load cannot disagree.

use crate::facts::FileReader;

use super::cgroup::parse_pressure_some_total;
use super::proc_stat::{PROC_STAT, parse_host_busy_usec};

/// The host's CPU pressure: how long some task of the host waited for a CPU.
pub const PROC_CPU_PRESSURE: &str = "/proc/pressure/cpu";

/// The whole host's CPU counters, at one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCpu {
    /// The time every CPU of the host was busy since boot, in microseconds, summed over the
    /// CPUs ([`parse_host_busy_usec`]).
    pub busy_usec: u64,
    /// How long some task of the host waited for a CPU, in microseconds, in all (the `some`
    /// total of `/proc/pressure/cpu`). `None` when that file cannot be read (a kernel without
    /// pressure accounting) or has no `some` total.
    pub pressure_some_usec: Option<u64>,
}

/// The host's CPU counters from `files`, or `None` when `/proc/stat` cannot be read or
/// [`parse_host_busy_usec`] finds no busy time in it (no zero is invented).
pub fn host_cpu(files: &dyn FileReader) -> Option<HostCpu> {
    let busy_usec = parse_host_busy_usec(&files.read(PROC_STAT).ok()?)?;
    let pressure = files.read(PROC_CPU_PRESSURE).ok();
    Some(HostCpu {
        busy_usec,
        pressure_some_usec: pressure.as_deref().and_then(parse_pressure_some_total),
    })
}
