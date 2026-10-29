//! The plain values a reading arrives as.
//!
//! The server converts the agent's wire form into these (this crate depends on no wire
//! crate), with every lease id already parsed to a [`LeaseId`]. They hold integers only.

use js_types::LeaseId;

/// The last minute index a lease series names. A record with this index stands for "this
/// minute or any later one": the agent folds every later minute into it with the maximum
/// of each field, so a receiver merges repeats of it by maximum.
pub const TAIL_MINUTE: u16 = 63;

/// One reading of a node's actions cgroup, with the lease records that came with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadIn {
    /// Counts readings within one agent process; each is one more than the last.
    pub seq: u64,
    /// The agent's monotonic clock in microseconds when the reading was taken.
    pub mono_usec: u64,
    /// Cumulative CPU time of the actions cgroup, in microseconds.
    pub cpu_usage_usec: u64,
    /// The working set of the actions cgroup, in bytes.
    pub working_set_bytes: u64,
    /// Whether the actions cgroup has a CPU limit.
    pub actions_cpu_limited: bool,
    /// Leases that ran for the whole window since the previous reading.
    pub covered: Vec<LeaseId>,
    /// Closed lease minutes, oldest first.
    pub minutes: Vec<MinuteIn>,
    /// When each running lease started, on the agent's clock.
    pub clocks: Vec<ClockIn>,
}

/// One closed minute of one lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinuteIn {
    /// The lease.
    pub lease: LeaseId,
    /// Minutes since the lease's start, 0 to [`TAIL_MINUTE`].
    pub minute: u16,
    /// Mean CPU over the minute's observed span, in millicores.
    pub mean_cores_milli: u32,
    /// The busiest window's CPU in the minute, in millicores; never below the mean.
    pub peak_window_milli: u32,
    /// The largest working set seen in the minute, in bytes.
    pub working_set_max_bytes: u64,
}

/// When one running lease started, on the node's own monotonic clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockIn {
    /// The lease.
    pub lease: LeaseId,
    /// Its start, in microseconds on the agent's clock.
    pub started_mono_usec: u64,
}
