//! The registry's side of the whole-node lease class (design `whole-node.md`).
//!
//! [`exclusive`] books the class on one host. [`caps`] and [`cost`] (from js-whole) are the pure rules of
//! whole-node placement (design `whole-node.md`, sections 3, 4 and 6).
//!
//! Nothing in those two holds state, reads a clock or takes a lock: each rule is a function of
//! plain structs and plain integers (milliseconds), so the scheduler that calls it under
//! the table lock decides on facts it has just read, and a test states a case without
//! building a pool. Mixed-pool inputs (ordinary hosts, older ordinary requests) are
//! modelled here even though only dedicated pools run the class today.
//!
//! - [`cost`]: which host a reservation drains first, and the deadline it gets.
//! - [`caps`]: the effective ceiling, the cap that blocks a reservation, the
//!   reservations to drop when the ceiling falls, and the last-capable-host rule.
//! - [`reserve`]: the state machine that uses them, marking hosts for waiting requests.
//!
//! Which pass marks hosts depends on the placement. With no placer (`--placement registry`,
//! the default), or a placer that keeps no queue, [`reserve`] runs at every pass. A placer
//! that keeps the queue ([`crate::Placer::whole_round`]: the engine of `--placement
//! js-sched`) runs the pass instead: the registry hands it the wait list, how it stands each
//! host and the expected life of each lease, and takes the marks it answers as its own
//! ([`reserve::Marks::adopt`]). The wait list, its cap and `regain_grace`, the request
//! records, the exclusions after failed prepares, the start's gates and the commit stay
//! the registry's under both. [`reserve`]'s pass is bypassed, not removed, under the engine:
//! it is removed only with the registry placement.

// The pure rules live in js-whole; these keep their old paths inside the registry.
pub(crate) use js_whole::{caps, cost};
pub(crate) mod ending;
pub(crate) mod exclusive;
pub(crate) mod placement;
pub(crate) mod queue;
pub(crate) mod reasons;
pub(crate) mod requests;
pub(crate) mod reserve;
