//! The whole-cell simulation of this repository, built in parts: the real lease server
//! under the [`js_sim`] kernel, many fake agents and a client driver, with properties
//! checked after every step, and a builder in which a scenario reads top to bottom.
//!
//! How to run, write and read a scenario, and what the cell does not model, is in
//! `docs/simulation.md` at the root of the repository.
//!
//! What it holds:
//!
//! - [`observe`]: the facts. What the client driver heard about each unit of work and
//!   what it asked for ([`ClientEvent`], [`Ask`]), the ground truth of each agent
//!   process ([`AgentTruth`]), the cell's own ledger of launches ([`Launch`]),
//!   completions ([`Completion`]) and verdicts ([`Acked`]), which outlives any agent
//!   process, the processes it killed ([`Killed`]), and the server's list of hosts
//!   ([`ServerView`]), gathered at one virtual time into [`Observations`].
//! - [`invariant`]: the oracle ([`Invariant`]), the [`Checker`] that tests it after each
//!   step and once at the end, and the one-line [`Violation`] it reports. Seven
//!   properties: no host booked past what it holds, no lease on two hosts and no host
//!   listed twice ([`Invariant::NoDoubleBooking`]); each run launched at most once
//!   ([`Invariant::AtMostOneLaunch`]); one terminal answer per item
//!   ([`Invariant::OneTerminalAnswer`]); no new lease on a host the server lists as lost
//!   ([`Invariant::NoLeaseOnALostHost`]); everything settled once quiet
//!   ([`Invariant::SettlesWhenQuiet`]); no booking without a run and no run without a
//!   booking at the end ([`Invariant::BookingsMatchAgents`]); and no result lost or
//!   recorded twice ([`Invariant::NoResultLost`]).
//! - [`bus`]: what an agent and the server send each other, one HTTP call as a request
//!   and its answer ([`Msg`]), and how a trace writes it.
//! - [`agent`]: the agent node ([`AgentNode`]), the conformance suite's in-process agent
//!   behind the real wire, which writes what it launches and completes and the verdicts
//!   it reads into the cell's [`Ledger`] and counts what happened to it ([`Counters`]).
//! - [`server`]: the real lease server in process as a kernel node ([`ServerNode`]), built
//!   from a [`ServerSpec`] by the server's own command-line parser, on the kernel's clock,
//!   and started again over its own files with `--boot resume` ([`ServerNode::restart`]).
//! - [`CellNode`]: the two kinds of node, as the kernel runs them.
//! - [`work`]: one unit of client work ([`Work`]): a run that lasts a fixed time, what it
//!   asks for, and when the client gives up on it.
//! - [`front`]: the client driver. A [`Front`] turns units of work into calls to the
//!   server and reports what it hears as [`ClientEvent`]s; [`LeaseFront`] does it as `js
//!   run` does today. The client is not a bus node: the run loop polls it at exact virtual
//!   times and its calls are never dropped.
//! - [`cell`]: the run loop ([`Cell`]), which steps the kernel, runs the actions scripted
//!   at virtual times, polls the client and checks the invariants after every step, with
//!   the changes a scenario makes (agents added, killed, killed for good and replaced,
//!   partitions, faults on the bus, a restart of the server, client calls, items
//!   submitted and cancelled), and the end of a scenario with its end check
//!   ([`Cell::settle`]).
//! - [`actions`]: what a scenario does at a virtual time, one [`Action`] per file.
//! - [`scenario`]: the builder ([`Scenario`]): the world, the work, the actions, what is
//!   expected ([`Expect`]) and the witnesses that show the situation happened, run over
//!   seeds. The smoke family (`tests/smoke.rs`) proves the machine works; the fault
//!   family (`tests/faults.rs`) runs agents that die and return or never return,
//!   partitions that heal, a lossy wire, a silent host, a stale start after its stop, a
//!   restart of the server and a forgotten host, each over sixteen seeds.
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

pub mod actions;
pub mod agent;
pub mod bus;
pub mod cell;
mod counters;
pub mod front;
pub mod invariant;
mod ledger;
mod node;
pub mod observe;
pub mod replay;
pub mod scenario;
pub mod server;
mod text;
pub mod work;

pub use actions::Action;
pub use agent::{AgentNode, AgentSpec, SIM_RUN_IMAGE};
pub use bus::{CallId, Msg, Route};
pub use cell::{Cell, Setup};
pub use counters::{Counter, Counters};
pub use front::{ClientPort, Front, FrontFactory, LeaseFront};
pub use invariant::{Checked, Checker, Invariant, NotSettled, Touched, Violation};
pub use ledger::{Acked, Completion, Ledger};
pub use node::CellNode;
pub use observe::{
    AgentTruth, Ask, ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Killed, Launch,
    Observations, RunId, Running, ServerHost, ServerView, Verdict,
};
pub use replay::{Failure, FailureReport, drive, run};
pub use scenario::{Expect, HostIs, Passed, Scenario, ScenarioFailure, SeedRun};
pub use server::{EPOCH_MS, ServerNode, ServerSpec, Shape};
pub use work::Work;
