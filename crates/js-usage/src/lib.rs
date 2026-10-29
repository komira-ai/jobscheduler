//! Measured usage: what nodes and leases used, minute by minute, kept for admission to
//! learn from.
//!
//! A node reports a reading of its actions cgroup (`js_types::sched::usage::NodeLoad`) on its heartbeat (the server turns the
//! wire form into this crate's own plain input types, so this crate depends on no wire
//! crate). This crate turns readings into minute series, stores them through
//! [`js_state::Store`], prunes them, and fits a per-action cost curve from the runs of each action key.
//!
//! Every public item is documented (the workspace lint fails the build otherwise). The
//! state machines are pure: they take time as an argument, read no clock and use ordered
//! collections only. The crate is not held to the pure layer's lints because it
//! persists, but it keeps to the same conventions.
//!
//! # Profiles
//!
//! [`ProfileStore`] keeps, per action key ([`ProfileKey`]), a ring of the last
//! [`RING_RUNS`] runs that ended on their own, one state-store row per key, bounded in
//! bytes by evicting the least recently used key. [`fit()`] turns a ring into the curve
//! admission charges: per minute since the lease's start, the P90 of the cores and the
//! maximum of the working set over the runs, widened by [`DILATE_MINUTES`] minutes each
//! way; a key with fewer than [`MIN_RUNS_FOR_CURVE`] runs has none. [`ProfileCurves`]
//! holds the fitted curves in memory and is a `js_types::sched::usage::CurveSource`, so a
//! key that is cold, idle for over [`STALE_DAYS`] days or unnamed is charged flat.
//!
//! The fit lives here and not in `js-sizing` because that crate is pure and has no
//! dependencies; its P90 is the same nearest-rank definition, so a flat and a curve charge
//! agree. Values are raw: the memory margin and rounding are admission's.
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

pub use error::{CodecError, MinuteTooLarge, UsageError};
pub use input::{ClockIn, LoadIn, MinuteIn, TAIL_MINUTE};
pub use minute::NodeMinute;
pub use prune::{NODE_RETENTION_MINUTES, PruneStats};
pub use recorder::{Counters, DropReason, Limits, Observed, Recorder};
pub use store::{NodeBuffer, NodeSeries};

mod codec;
mod convert;
mod curves;
mod fit;
mod key;
mod profile_store;
mod ring;
#[cfg(test)]
mod testkit;

pub use codec::{ROW_VERSION, RowError};
pub use curves::{KeyMap, ProfileCurves, STALE_DAYS};
pub use fit::{DILATE_MINUTES, Fit, MIN_RUNS_FOR_CURVE, fit, p90};
pub use key::{BadKey, MAX_KEY_LEN, ProfileKey};
pub use profile_store::{KEY_PREFIX, KeyProfile, ProfileError, ProfileStore};
pub use ring::{Admitted, BUCKETS, Bucket, RING_RUNS, Ring, Run, RunMinute, RunSeries};

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
