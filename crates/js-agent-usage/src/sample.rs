//! One reading of a node and of its leases, as plain values.

/// One lease's reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseSample {
    /// The lease's id.
    pub lease_id: String,
    /// When the lease started, on the sample's clock.
    pub started_usec: u64,
    /// The lease cgroup's `usage_usec`: a counter that only grows.
    pub cpu_usage_usec: u64,
    /// The lease's `memory.current` less its `inactive_file`.
    pub working_set_bytes: u64,
}

/// One reading of the node and of its leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The clock reading taken right after the CPU counters.
    pub mono_usec: u64,
    /// The actions cgroup's `usage_usec`. It is the cgroup's own counter, never a sum of
    /// the leases', because a removed descendant's usage stays in its parent's counter. A
    /// host with no control groups (a Mac) gives its whole busy time, as `host_cpu_usec`.
    pub cpu_usage_usec: u64,
    /// The whole host's CPU busy time since boot, in microseconds, from `/proc/stat` (on a
    /// Mac its CPU load ticks): every process of the host counts, the jobs and everything
    /// else. A counter that only grows.
    pub host_cpu_usec: u64,
    /// How long some task of the host waited for a CPU since boot, in microseconds: the `some`
    /// total of `/proc/pressure/cpu`, a counter that only grows. `None` when that file cannot be
    /// read (a kernel without pressure accounting) and on a Mac, which has none; the rest of
    /// the sample stands.
    pub host_cpu_pressure_some_usec: Option<u64>,
    /// The actions cgroup's `memory.current` less its `inactive_file`; on a host with no
    /// control groups (a Mac) its whole non-file memory, as `host_nonfile_bytes`.
    pub working_set_bytes: u64,
    /// The whole host's non-file memory, in bytes: `MemTotal - MemFree - (Buffers + Cached -
    /// Shmem)` of `/proc/meminfo` (on a Mac its anonymous pages less the purgeable ones, its
    /// wired pages and the compressor's). `None` when that file cannot be read or lacks a line; the
    /// rest of the sample stands.
    pub host_nonfile_bytes: Option<u64>,
    /// The actions cgroup's `active_file`.
    pub active_file_bytes: u64,
    /// Whether the actions cgroup has a CPU limit, from its `cpu.max`. A reader that cannot
    /// tell fails the whole sample instead of guessing.
    pub cpu_limited: bool,
    /// The leases that could be read.
    pub leases: Vec<LeaseSample>,
    /// How many leases were left out because a file could not be read or parsed.
    pub unreadable_leases: u32,
}
