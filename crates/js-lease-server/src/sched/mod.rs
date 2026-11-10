//! The server's side of placing work with `js-sched`.
//!
//! The placer itself is the `js-sched-placer` crate: [`map`] holds the pure conversions
//! between the registry's vocabulary and the scheduler's, [`place`] is the placer a registry
//! always takes, and [`engine`] is the one scheduler it keeps for the life of the server,
//! which follows the registry's books through the registry's own events and is asked at
//! each start which of the hosts that could take it to book. The engine also keeps the
//! registry's whole-node queue: it marks, commits and cancels the pool's whole-node
//! requests in place of the registry's own pass ([`engine::whole`]). They are named here under their
//! old paths. What stays in the server is the pass over the queued jobs that
//! the tick and the heartbeats run ([`jobs_pass`]), the machine provider each host it offers
//! is of ([`rank`]), and the tests that drive the placer through the server.

pub(crate) use js_sched_placer::{engine, map, place};

mod jobs_pass;
pub(crate) mod rank;

#[cfg(test)]
mod engine_hosts_tests;
#[cfg(test)]
mod engine_rig;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod walk_tests;
#[cfg(test)]
mod wiring_tests;
