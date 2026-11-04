//! The server's side of placing work with `js-sched`.
//!
//! [`map`] holds the pure conversions between this crate's vocabulary and the scheduler's.
//! [`place`] is the placer a registry takes under `--placement js-sched`: it keeps one
//! scheduler for the life of the server, [`engine`], which follows the registry's books
//! through the registry's own events and is asked at each start which of the hosts that
//! could take it to book. [`probe`] judges a probe's host by the scheduler's probe rule.

pub(crate) mod engine;
// `host_id` and the lease-text conversions are for the step that reads grants back.
#[allow(
    dead_code,
    reason = "the conversions back from the scheduler are not read yet"
)]
pub(crate) mod map;
pub(crate) mod place;
mod probe;

#[cfg(test)]
mod backfill_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod wiring_tests;
