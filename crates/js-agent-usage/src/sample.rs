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
    /// The clock reading taken right after the node's `cpu.stat`.
    pub mono_usec: u64,
    /// The actions cgroup's `usage_usec`. It is the cgroup's own counter, never a sum of
    /// the leases', because a removed descendant's usage stays in its parent's counter.
    pub cpu_usage_usec: u64,
    /// The actions cgroup's `memory.current` less its `inactive_file`.
    pub working_set_bytes: u64,
    /// The actions cgroup's `active_file`.
    pub active_file_bytes: u64,
    /// Whether the actions cgroup has a CPU limit. An unreadable or unparsable `cpu.max`
    /// counts as limited, the weaker reading.
    pub cpu_limited: bool,
    /// The leases that could be read.
    pub leases: Vec<LeaseSample>,
    /// How many leases were left out because a file could not be read or parsed.
    pub unreadable_leases: u32,
}
