//! The placer a registry takes at construction: the server's side of placing
//! work with `js-sched`, moved out of `js-lease-server` into a runtime-layer crate of its
//! own, which the server uses.
//!
//! [`map`] holds the pure conversions between the registry's vocabulary and the
//! scheduler's. [`place`] is the placer, [`place::SchedPlacer`]: it keeps one scheduler for
//! the life of the server, [`engine`], which follows the registry's hosts through the
//! registry's events and places the queued jobs ([`engine::jobs`]), those that need the whole
//! host included ([`engine::whole_jobs`]: it queues, marks and commits them from the job
//! tables).
//!
//! With the `test-support` feature, the placer and the engine also answer what a test reads
//! of them: what the scheduler books and charges on a host, and the scheduler itself.

pub mod engine;
pub mod map;
pub mod place;
