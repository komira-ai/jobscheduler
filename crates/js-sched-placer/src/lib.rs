//! The placer a registry takes at construction: the server's side of placing
//! work with `js-sched`, moved out of `js-lease-server` into a runtime-layer crate of its
//! own, which the server uses.
//!
//! [`map`] holds the pure conversions between the registry's vocabulary and the
//! scheduler's. [`place`] is the placer, [`place::SchedPlacer`]: it keeps one scheduler for
//! the life of the server, [`engine`], which follows the registry's books through the
//! registry's events and is asked at each start which of the hosts that could take it to
//! book; the same engine places the queued jobs ([`engine::jobs`]), those that need the whole
//! host included ([`engine::whole_jobs`]: it queues, marks and commits them from the job
//! tables). A probe's host is judged by the scheduler's probe rule. Under whole-node settings
//! the engine also keeps the registry's whole-node queue ([`engine::whole`]): it marks,
//! commits and cancels the pool's whole-node requests, costing a host to drain with the
//! registry's expected lives of its leases ([`lives`]).
//!
//! With the `test-support` feature, the placer and the engine also answer what a test reads
//! of them: the starts that fell back or found the engine out of date, the engines built,
//! what the scheduler books and charges on a host, and the scheduler itself.

pub mod engine;
pub mod lives;
pub mod map;
pub mod place;
mod probe;
