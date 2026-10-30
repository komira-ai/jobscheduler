//! The seam between the server and whatever measures usage: [`UsageSink`], the plain
//! values that cross it, and [`KeySource`].
//!
//! The seam is [`js_usage`]'s, so that any server can sit on it; this module names it for
//! the server and for the crate's public API. The server does the transport work (a
//! heartbeat's load is checked, its lease ids parsed, the node's identity taken from the
//! registry's answer, see `usage_map` and `usage_hub`) and hands plain values to a sink.
//! [`NoUsage`] is the sink that records nothing, and [`NoKeys`] the key source that knows
//! no run's key.

/// What the server knows of a node's capacity, for the sink's checks on a reading that
/// claims more than the machine has.
pub use js_usage::Limits as HostLimits;
/// One closed wall minute of one node.
pub use js_usage::NodeMinute as NodeRow;
pub use js_usage::{
    BadKey, ClockIn, DropReason, Fit, KeyProfile, KeySource, LoadIn, MAX_KEY_LEN, MinuteIn, NoKeys,
    NoUsage, Observation, Observed, ProfileCurves, ProfileKey, RunEnd, RunMinute, RunSeries,
    SinkCounters, UsageFault, UsageSink, drop_name,
};
