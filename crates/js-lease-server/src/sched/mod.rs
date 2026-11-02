//! The server's side of placing work with `js-sched`.
//!
//! [`map`] holds the pure conversions between this crate's vocabulary and the scheduler's.
//! [`place`] is the placer a registry takes under `--placement js-sched`: it builds a
//! scheduler over the hosts a start could book and asks it which one.

// `host_id` and the lease-text conversions are for the step that reads grants back.
#[allow(
    dead_code,
    reason = "the conversions back from the scheduler are not read yet"
)]
pub(crate) mod map;
pub(crate) mod place;

#[cfg(test)]
mod wiring_tests;
