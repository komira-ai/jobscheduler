//! The placer a registry takes under `--placement js-sched`: the server's side of placing
//! work with `js-sched`, moved out of `js-lease-server` into a runtime-layer crate of its
//! own, which the server uses.
//!
//! [`map`] holds the pure conversions between the registry's vocabulary and the
//! scheduler's. [`place`] is the placer, [`place::SchedPlacer`]: it keeps one scheduler for
//! the life of the server, [`engine`], which follows the registry's books through the
//! registry's own events and is asked at each start which of the hosts that could take it to
//! book; the same engine places the queued jobs ([`engine::jobs`]). A probe's host is judged
//! by the scheduler's probe rule.
//!
//! With the `test-support` feature, the placer and the engine also answer what a test reads
//! of them: the starts that fell back or found the engine out of date, the engines built,
//! what the scheduler books and charges on a host, and the scheduler itself.

pub mod engine;
pub mod map;
pub mod place;
mod probe;
