//! The whole-cell simulation of this repository, to be built in parts: the real lease
//! server under the [`js_sim`] kernel, many fake agents and a client driver, with
//! properties checked after every step.
//!
//! This first part holds no server, kernel, agent or driver. It is what the cell observes and
//! what must always be true of it, over plain values:
//!
//! - [`observe`]: the facts. What the client driver heard about each unit of work and
//!   what it asked for ([`ClientEvent`], [`Ask`]), the ground truth of each agent
//!   process ([`AgentTruth`]), the cell's own launch ledger ([`Launch`]), which outlives
//!   any agent process, and the server's list of hosts ([`ServerView`]), gathered at one
//!   virtual time into [`Observations`].
//! - [`invariant`]: the oracle. The five properties of "exactly once, nothing
//!   double-booked" ([`Invariant`]), the [`Checker`] that tests them after each step and
//!   once at the end, and the one-line [`Violation`] it reports.
//!
//! The invariants are defined against facts the cell reads from the agents and from the
//! client. The server's listing is read only where an invariant says so: a check that
//! reads what the server believes cannot catch the server believing something false.
//!
//! The crate keeps the kernel's rules although its layer does not demand them: no clock,
//! no random source and no hashed collection, so a run replays from its seed.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod invariant;
pub mod observe;
mod text;

pub use invariant::{Checked, Checker, Invariant, NotSettled, Touched, Violation};
pub use observe::{
    AgentTruth, Ask, ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Launch, Observations, RunId,
    Running, ServerHost, ServerView, Verdict,
};
