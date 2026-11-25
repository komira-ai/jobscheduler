//! What each lease costs the machine: CPU and memory sampled from the cgroup tree and cut
//! into per-lease minutes, ready to be sent to the server, beside the whole host's CPU busy
//! time from `/proc/stat` ([`host_cpu`]), which is what the server places by.
//!
//! [`Sampler`] reads cgroup files through a [`FileReader`](crate::FileReader) and takes the
//! time from a [`UsageClock`]; [`Binner`] (defined in the `js-agent-usage` crate, with
//! [`Sample`], and re-exported here) takes [`Sample`]s as values and reads nothing. No wall
//! time enters anywhere, so a step of the machine's clock cannot move a minute.
//!
//! # To the heartbeat
//!
//! The job worker owns a [`Collector`], which samples on the worker's thread once per
//! heartbeat interval (and, with `--host-second-samples`, reads the node's CPU and CPU
//! pressure every second, which the reading carries as two lists of rates) and publishes a [`Reading`] in an [`Outbox`]. The session takes the
//! reading into its next heartbeat ([`to_wire`]) and tells the outbox whether the beat went
//! through ([`Outbox::delivered`], [`Outbox::failed`], [`Outbox::refused`]), so a minute is
//! not sent again after a 2xx and a failed send is not lost.
//!
//! Delivery is at least once. When the server processed a beat but the reply was lost (a
//! transport error), the agent cannot know, keeps the minutes and sends them again; the
//! server dedupes by (lease, minute). Dropped minutes (too old, or refused) are logged by
//! the worker with their count, not sent. On Linux, without `--actions-cgroup` there is no
//! collector and the heartbeat carries no load. A Mac, which has no control groups, samples
//! its host's own counters ([`HostSampler`], `usage/macos.rs`): the load is the host's busy
//! time and non-file memory, with no lease minutes.
//!
//! The window is the heartbeat interval of the current session: when a later session is
//! welcomed with another interval, the sampling and the binner's gap rule follow it.
//!
//! # Time bases
//!
//! Every time here is microseconds on one monotonic clock (see [`UsageClock`]). A lease
//! carries the moment it started on that clock, and its minute `k` is the span
//! `[start + k * 60 s, start + (k + 1) * 60 s)`: minutes count from the lease's own start,
//! not from the node's.
//!
//! # Absent versus zero
//!
//! - A minute is **absent** when it was never observed, or observed for less than
//!   [`MIN_SPAN_USEC`]. Nothing is invented for it: no zero is filled in, so the server can
//!   tell "idle" from "unknown".
//! - A minute is **zero** only when it was observed and the lease used no CPU: it is
//!   reported with a mean and a peak of 0.
//! - A lease's first sample is its baseline. CPU used before it is attributed to no minute.
//! - A node file that cannot be read makes the whole [`Sample`] an error; a lease whose
//!   files cannot be read is left out of it and counted.
//!
//! # Long leases
//!
//! Minute indices of 63 and above share one tail bucket (`63`, "minute 63 or later"),
//! folded by the maximum of each field, so a lease holds at most 64 pending entries however
//! long it runs.

mod binner;
mod cgroup;
mod clock;
mod collect;
mod flags;
mod host;
mod host_sampler;
#[cfg(target_os = "macos")]
pub mod macos;
mod outbox;
mod sampler;
mod wire;

pub use binner::{
    Binner, ClosedMinute, Counters, MAX_UNSENT_AGE_USEC, MIN_SPAN_USEC, Observation, TAIL_MINUTE,
    WindowPart, window_parts,
};
pub use cgroup::{
    Layout, cpu_limited, layout_of, parse_active_file, parse_inactive_file,
    parse_pressure_some_total, parse_single_u64, parse_stat_key, parse_usage_usec, working_set,
};
pub use clock::{MINUTE_USEC, MonotonicClock, UsageClock};
pub use collect::Collector;
pub use flags::SampleFlags;
pub use host::{
    HostCpu, PROC_CPU_PRESSURE, PROC_MEMINFO, PROC_STAT, host_cpu, host_nonfile,
    parse_meminfo_nonfile_bytes, parse_proc_stat_busy_usec,
};
pub use host_sampler::{CpuTicks, HostSampler, HostStatistics, VmPages};
pub use js_agent_usage::SecondRates;
pub use outbox::{Outbox, Reading};
pub use sampler::{LeaseCgroup, LeaseSample, NodeSampler, Sample, SampleError, Sampler};
pub use wire::to_wire;
