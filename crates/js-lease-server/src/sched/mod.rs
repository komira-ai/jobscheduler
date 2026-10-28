//! The server's side of placing work with `js-sched`.
//!
//! Only the pure conversions between this crate's vocabulary and the scheduler's exist
//! so far ([`map`]). Nothing in the server calls them yet.

// The shell that consumes `map` lands with the placement flag; until it does every
// item is used by tests alone.
#[allow(dead_code, reason = "the scheduler shell that calls these lands later")]
pub(crate) mod map;
