//! Measured usage: what nodes and leases used, minute by minute, kept for admission to
//! learn from.
//!
//! A node reports a reading of its actions cgroup (`js_types::sched::usage::NodeLoad`) on its heartbeat (the server turns the
//! wire form into this crate's own plain input types, so this crate depends on no wire
//! crate). This crate turns readings into minute series, stores them through
//! [`js_state::Store`], prunes them, and (in later changes) fits a per-action cost curve.
//!
//! Every public item is documented (the workspace lint fails the build otherwise). The
//! state machines are pure: they take time as an argument, read no clock and use ordered
//! collections only. The crate is not held to the pure layer's lints because it
//! persists, but it keeps to the same conventions.
//!
//! # Time
//!
//! Three bases never mix: a node's monotonic microseconds (windows and lease minutes,
//! never compared across nodes), wall minutes since the Unix epoch (the rows of a node
//! series; the caller supplies them), and the store's own clock (expiry, not used here).

pub mod error;
pub mod input;
pub mod minute;
pub mod prune;
pub mod recorder;
pub mod store;

pub use error::{CodecError, UsageError};
pub use input::{ClockIn, LoadIn, MinuteIn, TAIL_MINUTE};
pub use minute::NodeMinute;
pub use prune::{NODE_RETENTION_MINUTES, PruneStats};
pub use recorder::{Counters, DropReason, Limits, Observed, Recorder};
pub use store::{NodeBuffer, NodeSeries};

/// Wall minutes in an hour: the unit a node series is stored in (one row per hour).
pub const MINUTES_PER_HOUR: u64 = 60;

/// The hour since the epoch that wall minute `minute` falls in.
#[must_use]
pub fn hour_of(minute: u64) -> u64 {
    minute / MINUTES_PER_HOUR
}

/// How many hour rows the wall minutes `from..=to` touch; 0 when `to` is before `from`.
#[must_use]
pub fn hours_spanned(from: u64, to: u64) -> u64 {
    if to < from {
        0
    } else {
        hour_of(to) - hour_of(from) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::{hour_of, hours_spanned};

    // Catches an hour boundary off by one: minute 59 is hour 0, minute 60 is hour 1.
    #[test]
    fn an_hour_ends_at_its_sixtieth_minute() {
        assert_eq!(hour_of(0), 0);
        assert_eq!(hour_of(59), 0);
        assert_eq!(hour_of(60), 1);
        assert_eq!(hour_of(u64::MAX), u64::MAX / 60);
    }

    // Catches a span that counts the minutes and not the hours, and one that underflows
    // when the range is reversed.
    #[test]
    fn a_range_counts_the_hours_it_touches() {
        assert_eq!(hours_spanned(59, 60), 2);
        assert_eq!(hours_spanned(60, 119), 1);
        assert_eq!(hours_spanned(5, 5), 1);
        assert_eq!(hours_spanned(6, 5), 0);
    }
}
