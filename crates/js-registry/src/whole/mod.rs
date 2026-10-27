//! The registry's side of the whole-node lease class (design `whole-node.md`).
//!
//! [`exclusive`] books the class on one host. [`caps`] and [`cost`] are the pure rules of
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

// The last-capable-host rule and the mixed-pool inputs are modelled for the mixed-pool
// step; a dedicated pool, the only one that runs the class today, never reads them.
#[allow(dead_code, reason = "mixed-pool inputs, used by a later step")]
pub(crate) mod caps;
pub(crate) mod cost;
pub(crate) mod exclusive;
pub(crate) mod queue;
pub(crate) mod reasons;
pub(crate) mod requests;
pub(crate) mod reserve;
