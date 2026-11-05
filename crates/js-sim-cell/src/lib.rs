//! The whole-cell simulation of this repository, to be built in parts: the real lease
//! server under the [`js_sim`] kernel, many fake agents and a client driver, with
//! properties checked after every step.
//!
//! It holds no client driver yet. What it holds:
//!
//! - [`observe`]: the facts. What the client driver heard about each unit of work and
//!   what it asked for ([`ClientEvent`], [`Ask`]), the ground truth of each agent
//!   process ([`AgentTruth`]), the cell's own launch ledger ([`Launch`]), which outlives
//!   any agent process, and the server's list of hosts ([`ServerView`]), gathered at one
//!   virtual time into [`Observations`].
//! - [`invariant`]: the oracle. The five properties of "exactly once, nothing
//!   double-booked" ([`Invariant`]), the [`Checker`] that tests them after each step and
//!   once at the end, and the one-line [`Violation`] it reports.
//! - [`bus`]: what an agent and the server send each other, one HTTP call as a request
//!   and its answer ([`Msg`]), and how a trace writes it.
//! - [`agent`]: the agent node ([`AgentNode`]), the conformance suite's in-process agent
//!   behind the real wire, which writes what it launches and completes into the cell's
//!   [`Ledger`] and counts what happened to it ([`Counters`]).
//! - [`server`]: the real lease server in process as a kernel node ([`ServerNode`]), built
//!   from a [`ServerSpec`] by the server's own command-line parser, on the kernel's clock.
//! - [`CellNode`]: the two kinds of node, as the kernel runs them.
//! - [`cell`]: the run loop ([`Cell`]), which steps the kernel, runs the actions scripted
//!   at virtual times and checks the invariants after every step, with the changes a
//!   scenario makes (agents added, killed and replaced, partitions, faults on the bus,
//!   client calls).
//! - [`replay`]: a seed run to its end, run again with its trace kept when it fails, and the
//!   [`FailureReport`] that names the scenario, the seed, the trace hash, the violation
//!   and the last trace lines.
//!
//! The invariants are defined against facts the cell reads from the agents and from the
//! client. The server's listing is read only where an invariant says so: a check that
//! reads what the server believes cannot catch the server believing something false.
//!
//! The crate keeps the kernel's rules although its layer does not demand them: no clock,
//! no random source and no hashed collection, so a run replays from its seed. The server
//! it runs is production code and keeps them only where the cell gives it the means: it
//! reads the cell's clock, its lease table times its saves on that clock, and usage
//! recording, which reads the system's clock, is off. Its session secrets are random;
//! the agents echo them, and no trace line holds one.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod agent;
pub mod bus;
pub mod cell;
mod counters;
pub mod invariant;
mod ledger;
mod node;
pub mod observe;
pub mod replay;
pub mod server;
mod text;

pub use agent::{AgentNode, AgentSpec, SIM_RUN_IMAGE};
pub use bus::{CallId, Msg, Route};
pub use cell::{Cell, Setup};
pub use counters::{Counter, Counters};
pub use invariant::{Checked, Checker, Invariant, NotSettled, Touched, Violation};
pub use ledger::{Completion, Ledger};
pub use node::CellNode;
pub use observe::{
    AgentTruth, Ask, ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Launch, Observations, RunId,
    Running, ServerHost, ServerView, Verdict,
};
pub use replay::{Failure, FailureReport, run};
pub use server::{EPOCH_MS, ServerNode, ServerSpec, Shape};
