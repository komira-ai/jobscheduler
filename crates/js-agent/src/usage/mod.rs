//! What each lease costs the machine: CPU and memory sampled from the cgroup tree and cut
//! into per-lease minutes, ready to be sent to the server.
//!
//! The module is pure. [`Sampler`] reads cgroup files through a [`FileReader`](crate::FileReader)
//! and takes the time from a [`UsageClock`]; [`Binner`] (defined in the `js-agent-usage`
//! crate, with [`Sample`], and re-exported here) takes [`Sample`]s as values and reads
//! nothing. No wall time enters anywhere, so a step of the machine's clock cannot move a
//! minute.
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
mod sampler;

pub use binner::{
    Binner, ClosedMinute, Counters, MAX_UNSENT_AGE_USEC, MIN_SPAN_USEC, Observation, TAIL_MINUTE,
    WindowPart, window_parts,
};
pub use cgroup::{
    cpu_limited, parse_active_file, parse_inactive_file, parse_single_u64, parse_stat_key,
    parse_usage_usec, working_set,
};
pub use clock::{MINUTE_USEC, MonotonicClock, UsageClock};
pub use sampler::{LeaseCgroup, LeaseSample, Sample, SampleError, Sampler};
