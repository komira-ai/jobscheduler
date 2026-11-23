//! The whole host's CPU ([`host_cpu`]): the busy time of every CPU from `/proc/stat` and the
//! `some` total of `/proc/pressure/cpu`, read through a [`FileReader`]. One reader, so that every
//! number the heartbeat gives for the host counts the same CPU time. And the whole host's memory
//! in use ([`host_memory_used`]), from `/proc/meminfo`.

use crate::facts::FileReader;

use super::cgroup::parse_pressure_some_total;

/// The kernel's CPU accounting, with the time every CPU of the host was busy.
pub const PROC_STAT: &str = "/proc/stat";
/// The host's CPU pressure: how long some task of the host waited for a CPU.
pub const PROC_CPU_PRESSURE: &str = "/proc/pressure/cpu";
/// The kernel's memory accounting, with the host's total and available memory.
pub const PROC_MEMINFO: &str = "/proc/meminfo";
/// Microseconds in one tick of `/proc/stat`. The kernel reports it in `USER_HZ`, which is 100 a
/// second in its user interface on every architecture Linux builds for.
const USEC_PER_TICK: u64 = 10_000;

/// The whole host's CPU counters, at one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCpu {
    /// The time every CPU of the host was busy, in microseconds, summed over the CPUs.
    pub busy_usec: u64,
    /// How long some task of the host waited for a CPU, in microseconds, in all (the `some`
    /// total of `/proc/pressure/cpu`). `None` when that file cannot be read (a kernel without
    /// pressure accounting).
    pub pressure_some_usec: Option<u64>,
}

/// The host's CPU counters from `files`, or `None` when `/proc/stat` cannot be read or has no
/// `cpu` line (no zero is invented).
pub fn host_cpu(files: &dyn FileReader) -> Option<HostCpu> {
    let busy_usec = parse_proc_stat_busy_usec(&files.read(PROC_STAT).ok()?)?;
    let pressure = files.read(PROC_CPU_PRESSURE).ok();
    Some(HostCpu {
        busy_usec,
        pressure_some_usec: pressure.as_deref().and_then(parse_pressure_some_total),
    })
}

/// The busy time of every CPU from the text of `/proc/stat`, in microseconds: the `user`,
/// `nice`, `system`, `irq`, `softirq` and `steal` ticks of its first line, `cpu`, the sum over
/// the CPUs. `idle` and `iowait` are not busy; `guest` and `guest_nice` are already in `user`
/// and `nice`. `None` for text whose first line is not `cpu` followed by at least those eight
/// numbers.
pub fn parse_proc_stat_busy_usec(text: &str) -> Option<u64> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let ticks: Vec<u64> = fields.map_while(|field| field.parse().ok()).collect();
    let [user, nice, system, _idle, _iowait, irq, softirq, steal, ..] = ticks[..] else {
        return None;
    };
    let busy = [user, nice, system, irq, softirq, steal]
        .into_iter()
        .fold(0_u64, u64::saturating_add);
    Some(busy.saturating_mul(USEC_PER_TICK))
}

/// The memory in use on the whole host from `files`, in bytes ([`parse_meminfo_used_bytes`]), or
/// `None` when `/proc/meminfo` cannot be read or lacks a line it needs (no zero is invented).
pub fn host_memory_used(files: &dyn FileReader) -> Option<u64> {
    parse_meminfo_used_bytes(&files.read(PROC_MEMINFO).ok()?)
}

/// The memory in use on the whole host from the text of `/proc/meminfo`, in bytes: `MemTotal`
/// less `MemAvailable` (both in kibibytes), 0 when the second is the larger. `MemAvailable` is
/// the kernel's figure of what can be given to a new process without swapping, page cache it can
/// drop included, so what is left is memory every process of the host holds. `None` when either
/// line is missing or not a number.
pub fn parse_meminfo_used_bytes(text: &str) -> Option<u64> {
    let kib = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
    };
    let used = kib("MemTotal:")?.saturating_sub(kib("MemAvailable:")?);
    Some(used.saturating_mul(1024))
}
