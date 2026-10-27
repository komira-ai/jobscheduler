//! The pure rules of whole-node placement (design whole-node.md, sections 3, 4 and 6).
//!
//! Nothing here holds state, reads a clock or takes a lock: each rule is a function of
//! plain structs and plain integers (milliseconds), so the scheduler that calls it under
//! the table lock decides on facts it has just read, and a test states a case without
//! building a pool. Mixed-pool inputs (ordinary hosts, older ordinary requests) are
//! modelled here even though only dedicated pools run the class today.
//!
//! - [`cost`]: which host a reservation drains first, and the deadline it gets.
//! - [`caps`]: the effective ceiling, the cap that blocks a reservation, the
//!   reservations to drop when the ceiling falls, and the last-capable-host rule.

pub mod caps;
pub mod cost;

pub use caps::{
    Blocked, Capability, Counts, Limits, Mark, MarkKind, OrdinaryRequest, capable_hosts, ceiling,
    ceiling_excess, last_capable_blocks, may_reserve,
};
pub use cost::{DrainCost, HostLoad, drain_cost, reserve_deadline};
