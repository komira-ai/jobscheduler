//! What a lease cgroup counted, read when the action ended (`cpu.stat`, `memory.peak`,
//! `memory.numa_stat`) into the [`CgroupUsage`] a lease's result carries.
//!
//! Measurement only: nothing here decides anything. A file that is not there (cgroup
//! v1, a kernel without `memory.peak`, a build without NUMA) leaves its fields absent,
//! and a file that does not parse does too; neither is an error, since the lease's
//! result does not depend on them.
//!
//! The NUMA split is the one a sampler saw while the action ran, if it held more than
//! at its end: the kernel uncharges anonymous memory when the processes exit, so
//! `memory.numa_stat` after the end shows little more than page cache ([`NumaSample`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often [`NumaSample::watch`] reads `memory.numa_stat` while the action runs, unless
/// the caller chooses another period.
pub const NUMA_EVERY: Duration = Duration::from_secs(1);

/// The counters of a lease's cgroup v2, read when the action ended. A field that is
/// `None` was not readable (no cgroup v2, or a controller not enabled): absent is not
/// zero. Measurement only: nothing in scheduling reads these.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CgroupUsage {
    /// `cpu.stat` `usage_usec`: CPU time of every process in the cgroup, in microseconds.
    pub cpu_usage_micros: Option<u64>,
    /// `cpu.stat` `user_usec`.
    pub cpu_user_micros: Option<u64>,
    /// `cpu.stat` `system_usec`.
    pub cpu_system_micros: Option<u64>,
    /// `cpu.stat` `nr_periods`: the CFS bandwidth periods the cgroup ran in. Zero without
    /// a `cpu.max` quota.
    pub cpu_periods: Option<u64>,
    /// `cpu.stat` `nr_throttled`: the periods it was throttled in.
    pub cpu_throttled_periods: Option<u64>,
    /// `cpu.stat` `throttled_usec`: the time it spent throttled.
    pub cpu_throttled_micros: Option<u64>,
    /// `memory.peak`: the most memory the cgroup held at once, in bytes.
    pub memory_peak_bytes: Option<u64>,
    /// `memory.numa_stat` anon + file per NUMA node, in bytes, of the sample with the most
    /// memory (the action's end, or an earlier sample while it ran: anon memory is gone
    /// once the processes have exited). Empty when not readable.
    pub numa: Vec<NumaNodeBytes>,
    /// `memory.events` `max`: how many times usage reached the lease's `memory.max`. Above
    /// zero, `memory_peak_bytes` is a floor on what the action needed, not the need (the
    /// lease was reclaimed or swapped at its limit): the sample is censored.
    pub memory_events_max: Option<u64>,
    /// `memory.events` `oom_kill`: processes of the lease the kernel OOM killer killed.
    pub memory_events_oom_kill: Option<u64>,
    /// `memory.swap.peak`, in bytes: the most swap the lease held at once. Absent on a
    /// kernel before 6.5, which has no such file.
    pub memory_swap_peak_bytes: Option<u64>,
    /// The largest anon + swap (`memory.stat` anon plus `memory.swap.current`) of any
    /// sample taken while the action ran, in bytes: what the action touched without the
    /// page cache `memory_peak_bytes` counts. A sample every second, so a floor on the
    /// true peak. Absent when either file was unreadable at every sample.
    pub memory_anon_swap_peak_bytes: Option<u64>,
    /// `cpu.pressure` `some` total, in microseconds: how long some task of the lease
    /// waited for a CPU while it ran. Absent without pressure stall information.
    pub cpu_pressure_some_micros: Option<u64>,
    /// `memory.pressure` `some` total, in microseconds: how long some task of the lease
    /// stalled on memory.
    pub memory_pressure_some_micros: Option<u64>,
    /// `memory.pressure` `full` total, in microseconds: how long every task stalled.
    pub memory_pressure_full_micros: Option<u64>,
    /// How many CPUs the lease's cgroup could run on (`cpuset.cpus.effective`): fewer than
    /// the node's for a lease placed on one NUMA node. Absent when not readable.
    pub cpus_effective: Option<u32>,
}

/// Bytes of one NUMA node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumaNodeBytes {
    /// The node's number.
    pub node: u32,
    /// Its bytes.
    pub bytes: u64,
}

/// The counters of `cpu.stat` the usage takes.
#[derive(Debug, Default, PartialEq, Eq)]
struct CpuStat {
    usage: Option<u64>,
    user: Option<u64>,
    system: Option<u64>,
    periods: Option<u64>,
    throttled: Option<u64>,
    throttled_usec: Option<u64>,
}

/// The `key value` lines of `cpu.stat`: each wanted key's value that parses.
fn cpu_stat(text: &str) -> CpuStat {
    let mut stat = CpuStat::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "usage_usec" => stat.usage = Some(value),
            "user_usec" => stat.user = Some(value),
            "system_usec" => stat.system = Some(value),
            "nr_periods" => stat.periods = Some(value),
            "nr_throttled" => stat.throttled = Some(value),
            "throttled_usec" => stat.throttled_usec = Some(value),
            _ => {}
        }
    }
    stat
}

/// The `max` and `oom_kill` counts of a `memory.events`.
fn memory_events(text: &str) -> (Option<u64>, Option<u64>) {
    let count = |key: &str| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == key).then(|| words.next()?.parse().ok())?
        })
    };
    (count("max"), count("oom_kill"))
}

/// The `total` (microseconds stalled) of the `kind` (`some` or `full`) line of a
/// pressure file (`some avg10=0.00 avg60=0.00 avg300=0.00 total=4120`).
fn pressure_total(text: &str, kind: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("total="))?
        .parse()
        .ok()
}

/// How many CPUs a `cpuset.cpus.effective` list (`0-3,8,10-11`) names; `None` for an
/// empty or malformed list.
fn cpu_list_len(text: &str) -> Option<u32> {
    let mut count: u32 = 0;
    for part in text.trim().split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((lo, hi)) => (lo.parse::<u32>().ok()?, hi.parse::<u32>().ok()?),
            None => {
                let one = part.parse::<u32>().ok()?;
                (one, one)
            }
        };
        count = count.checked_add(hi.checked_sub(lo)?.checked_add(1)?)?;
    }
    Some(count)
}

/// The `anon` bytes of a `memory.stat`.
fn anon_bytes(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "anon").then(|| words.next()?.parse().ok())?
    })
}

/// `anon` plus `file` bytes per node of a `memory.numa_stat` (lines such as
/// `anon N0=4096 N1=8192`; cgroup v2 counts bytes). Empty if neither line is there.
fn numa_stat(text: &str) -> BTreeMap<u32, u64> {
    let mut nodes = BTreeMap::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("anon" | "file")) {
            continue;
        }
        for word in words {
            let Some((node, bytes)) = word.strip_prefix('N').and_then(|w| w.split_once('=')) else {
                continue;
            };
            if let (Ok(node), Ok(bytes)) = (node.parse::<u32>(), bytes.parse::<u64>()) {
                let held: &mut u64 = nodes.entry(node).or_default();
                *held = held.saturating_add(bytes);
            }
        }
    }
    nodes
}

fn total(nodes: &BTreeMap<u32, u64>) -> u64 {
    nodes.values().fold(0, |a, b| a.saturating_add(*b))
}

/// The number in the file `name` of the cgroup `dir`, if it is there and parses.
fn read_u64(dir: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(dir.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The NUMA split of the sample that held the most memory.
#[derive(Debug, Default)]
pub struct NumaSample {
    best: BTreeMap<u32, u64>,
    /// The largest anon + swap any sample read.
    anon_swap_peak: Option<u64>,
    /// Counts each sample that read some memory, if the runtime counts them.
    counted: Option<Arc<AtomicU64>>,
}

impl NumaSample {
    /// A sample that adds one to `counted` for each reading of `memory.numa_stat` that
    /// held memory.
    pub fn counting(counted: Arc<AtomicU64>) -> Self {
        Self {
            best: BTreeMap::new(),
            anon_swap_peak: None,
            counted: Some(counted),
        }
    }

    /// Reads `memory.numa_stat` of the cgroup `dir` and keeps it if it holds more than
    /// every earlier sample (a sample that cannot be read changes nothing).
    pub fn observe(&mut self, dir: &Path) {
        // Anon + swap is what the action touched, without the page cache `memory.peak`
        // counts; both files must be there, or the sample is not anon + swap.
        if let (Some(anon), Some(swap)) = (
            std::fs::read_to_string(dir.join("memory.stat"))
                .ok()
                .and_then(|text| anon_bytes(&text)),
            read_u64(dir, "memory.swap.current"),
        ) {
            let now = anon.saturating_add(swap);
            self.anon_swap_peak = Some(self.anon_swap_peak.map_or(now, |p| p.max(now)));
        }
        let Ok(text) = std::fs::read_to_string(dir.join("memory.numa_stat")) else {
            return;
        };
        let now = numa_stat(&text);
        if let Some(counted) = &self.counted {
            counted.fetch_add(u64::from(total(&now) > 0), Ordering::Release);
        }
        if total(&now) > total(&self.best) {
            self.best = now;
        }
    }

    /// Samples every `every` (the driver passes [`NUMA_EVERY`]), for as long as it is
    /// polled.
    pub async fn watch(&mut self, dir: &Path, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            self.observe(dir);
        }
    }

    /// The usage of the cgroup `dir`, read after the action's processes have exited
    /// (the cgroup is not removed yet), with the best NUMA sample: the end counts as
    /// one more sample.
    pub fn finish(mut self, dir: &Path) -> CgroupUsage {
        self.observe(dir);
        let stat = std::fs::read_to_string(dir.join("cpu.stat"))
            .map(|text| cpu_stat(&text))
            .unwrap_or_default();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        let events = read("memory.events")
            .map(|text| memory_events(&text))
            .unwrap_or_default();
        let cpu_pressure = read("cpu.pressure");
        let memory_pressure = read("memory.pressure");
        CgroupUsage {
            cpu_usage_micros: stat.usage,
            cpu_user_micros: stat.user,
            cpu_system_micros: stat.system,
            cpu_periods: stat.periods,
            cpu_throttled_periods: stat.throttled,
            cpu_throttled_micros: stat.throttled_usec,
            memory_peak_bytes: read_u64(dir, "memory.peak"),
            memory_events_max: events.0,
            memory_events_oom_kill: events.1,
            memory_swap_peak_bytes: read_u64(dir, "memory.swap.peak"),
            memory_anon_swap_peak_bytes: self.anon_swap_peak,
            cpu_pressure_some_micros: cpu_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "some")),
            memory_pressure_some_micros: memory_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "some")),
            memory_pressure_full_micros: memory_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "full")),
            cpus_effective: std::fs::read_to_string(dir.join("cpuset.cpus.effective"))
                .ok()
                .and_then(|text| cpu_list_len(&text)),
            numa: self
                .best
                .into_iter()
                .map(|(node, bytes)| NumaNodeBytes { node, bytes })
                .collect(),
        }
    }
}

/// Where the kernel reports CPU pressure stall information.
pub const CPU_PSI: &str = "/proc/pressure/cpu";

/// What the lease looked like when it started, for the usage it reports: the node's CPU
/// pressure then and the CPU the scheduler booked. A sample of CPU use taken on a busy
/// node understates the need, so the estimator reads the pressure beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Start {
    /// `some avg10` of the CPU pressure file, in hundredths of a percent; `None` when
    /// the file is missing or does not parse (a kernel without pressure information).
    pub cpu_pressure_centi: Option<u32>,
    /// The CPU booked for the lease, in thousandths of a CPU; `None` when not booked.
    pub booked_millicpus: Option<u64>,
}

impl Start {
    /// The start of a lease that booked `millicpus` (0 is not booked), reading the CPU
    /// pressure file at `psi`.
    pub fn read(psi: &Path, millicpus: u64) -> Self {
        Self {
            cpu_pressure_centi: std::fs::read_to_string(psi)
                .ok()
                .and_then(|text| some_avg10_centi(&text)),
            booked_millicpus: (millicpus > 0).then_some(millicpus),
        }
    }
}

/// The `avg10` of the `some` line of a pressure file (`some avg10=1.23 avg60=0.40 ...`)
/// in hundredths of a percent (123); digits past the second decimal are dropped. `None`
/// if there is no such line or the value is not a plain decimal.
fn some_avg10_centi(text: &str) -> Option<u32> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?;
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let fraction = format!("{fraction:0<2}");
    let centi: u32 = fraction[..2].parse().ok()?;
    whole
        .parse::<u32>()
        .ok()?
        .checked_mul(100)?
        .checked_add(centi)
}

#[cfg(test)]
#[path = "cgroup_usage_tests.rs"]
mod tests;
