//! The registry's side of the whole-node lease class (design `whole-node.md`).
//!
//! - [`exclusive`]: books the class on one host, and the host's prepare state.
//! - [`caps`] (from js-whole): the effective ceiling and the cap that blocks a reservation,
//!   which the start reads for a whole-node start without a request id. A pure rule: it
//!   holds no state, reads no clock and takes no lock.
//! - [`reserve`]: a host set aside for a waiting request, as the registry shows it.
//! - [`requests`]: what is remembered of a request between its placements.
//! - [`placement`]: the answer to a whole-node start that found no free host.
//! - [`ending`] and [`reasons`]: a committed lease whose prepare failed.
//!
//! The placer keeps the queue (the engine of `js-sched-placer`): the wait list with its cap
//! and `regain_grace`, the request records, the exclusions and the marks, and runs the pass
//! over how the registry stands each host and the expected life of each lease
//! ([`crate::Placer::whole_round`]). The start's gates, the commit of the booking and the
//! host's prepare state stay the registry's.

// The pure rule lives in js-whole; this keeps its old path inside the registry.
pub(crate) use js_whole::caps;
pub(crate) mod ending;
pub(crate) mod exclusive;
pub(crate) mod placement;
pub(crate) mod reasons;
pub(crate) mod requests;
pub(crate) mod reserve;
