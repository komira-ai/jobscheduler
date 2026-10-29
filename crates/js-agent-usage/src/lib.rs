//! Cuts a node's readings into each lease's per-minute CPU and memory.
//!
//! The crate is pure and has no dependency: [`Binner`] takes [`Sample`]s as values and
//! reads no file, no clock and no socket. Any program that can read a node's counters can
//! cut them into minutes with it; `js-agent` does so from the cgroup tree, and a program
//! that must not link an HTTP client takes only this crate.
//!
//! # Time bases
//!
//! Every time is microseconds on one monotonic clock, the clock the samples are stamped
//! with. A lease carries the moment it started on that clock, and its minute `k` is the
//! span `[start + k * 60 s, start + (k + 1) * 60 s)`: minutes count from the lease's own
//! start, not from the node's. No wall time enters anywhere.
//!
//! # Absent versus zero
//!
//! - A minute is **absent** when it was never observed, or observed for less than
//!   [`MIN_SPAN_USEC`]. Nothing is invented for it: no zero is filled in, so a reader can
//!   tell "idle" from "unknown".
//! - A minute is **zero** only when it was observed and the lease used no CPU: it is
//!   reported with a mean and a peak of 0.
//! - A lease's first sample is its baseline. CPU used before it is attributed to no minute.
//!
//! # Long leases
//!
//! Minute indices of [`TAIL_MINUTE`] and above share one tail bucket ("minute 63 or
//! later"), folded by the maximum of each field, so a lease holds at most 64 pending
//! entries however long it runs.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod binner;
mod sample;

pub use binner::{
    Binner, ClosedMinute, Counters, MAX_UNSENT_AGE_USEC, MIN_SPAN_USEC, Observation, TAIL_MINUTE,
    WindowPart, window_parts,
};
pub use sample::{LeaseSample, Sample};

/// One minute in microseconds.
pub const MINUTE_USEC: u64 = 60_000_000;
