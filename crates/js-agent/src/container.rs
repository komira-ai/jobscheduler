//! What the process may use of the machine, when that is less than the machine has: the
//! limits of its cgroup (version 2) and the CPUs it may run on. Linux only; each function
//! reads through the [`FileReader`] seam and answers `None` for "no limit found", which is
//! also what an absent, unreadable or unparseable file means.

use crate::facts::FileReader;

/// Where the cgroup v2 hierarchy is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A warning when `/proc/self/cgroup` has cgroup v1 lines and no version 2 line: the limits
/// such a process may be under are not read, so the offer is the machine's totals.
pub(crate) fn warning(files: &dyn FileReader) -> Option<String> {
    files
        .read("/proc/self/cgroup")
        .ok()
        .filter(|text| {
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            !lines.is_empty() && lines.iter().all(|line| !line.starts_with("0::"))
        })
        .map(|_| {
            "/proc/self/cgroup is cgroup v1, whose limits are not read; offering the machine's totals"
                .to_owned()
        })
}

/// The directories of the cgroup the process is in and of each cgroup above it, up to the
/// root: a limit on any of them binds the process. Empty when `/proc/self/cgroup` has no
/// version 2 line (`0::<path>`).
fn cgroup_dirs(files: &dyn FileReader) -> Vec<String> {
    let own = files.read("/proc/self/cgroup").ok().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(str::to_owned)
    });
    own.map(|path| {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        (0..=parts.len())
            .rev()
            .map(|depth| {
                let below: String = parts[..depth]
                    .iter()
                    .map(|part| format!("/{part}"))
                    .collect();
                format!("{CGROUP_ROOT}{below}")
            })
            .collect()
    })
    .unwrap_or_default()
}

/// The lowest value `parse` reads from the file `name` of any of the cgroup directories.
fn lowest(files: &dyn FileReader, name: &str, parse: fn(&str) -> Option<u64>) -> Option<u64> {
    cgroup_dirs(files)
        .iter()
        .filter_map(|dir| files.read(&format!("{dir}/{name}")).ok())
        .filter_map(|text| parse(&text))
        .min()
}

/// The CPUs the process may use, in thousandths of a core: the lowest of the CPU quotas on
/// its cgroup path and the number of CPUs it may run on.
pub(crate) fn cpu_millis(files: &dyn FileReader) -> Option<u64> {
    let pinned = files
        .read("/proc/self/status")
        .ok()
        .and_then(|text| allowed_cpus(&text))
        .map(|cpus| cpus.saturating_mul(1000));
    lowest(files, "cpu.max", quota_millis)
        .into_iter()
        .chain(pinned)
        .min()
}

/// The memory the process may use, in bytes: the lowest `memory.max` on its cgroup path.
pub(crate) fn memory_bytes(files: &dyn FileReader) -> Option<u64> {
    lowest(files, "memory.max", limit_bytes)
}

/// `cpu.max` is `<quota> <period>` in microseconds, or `max <period>` for no quota.
fn quota_millis(text: &str) -> Option<u64> {
    let mut words = text.split_whitespace();
    let quota = words.next().and_then(|word| word.parse::<u64>().ok());
    let period = words
        .next()
        .and_then(|word| word.parse::<u64>().ok())
        .filter(|period| *period > 0);
    quota
        .zip(period)
        .map(|(quota, period)| quota.saturating_mul(1000) / period)
}

/// `memory.max` is a number of bytes, or `max`.
fn limit_bytes(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// The number of CPUs in the `Cpus_allowed_list` line of `/proc/self/status`, a list of
/// CPU numbers and ranges such as `0-3,8,10-11`.
fn allowed_cpus(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .and_then(|list| {
            list.trim().split(',').try_fold(0u64, |total, range| {
                cpus_in(range).map(|n| total.saturating_add(n))
            })
        })
}

/// The CPUs in one entry of the list: `5` is one, `2-4` is three, `4-2` is not a range.
fn cpus_in(entry: &str) -> Option<u64> {
    let (low, high) = entry.split_once('-').unwrap_or((entry, entry));
    low.parse::<u64>()
        .ok()
        .zip(high.parse::<u64>().ok())
        .filter(|(low, high)| low <= high)
        .map(|(low, high)| (high - low).saturating_add(1))
}
