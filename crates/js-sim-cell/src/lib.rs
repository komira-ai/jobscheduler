//! The whole-cell simulation of this repository, built in parts: the real server and its
//! job service under the [`js_sim`] kernel, many fake agents (or a few real ones) and a client driver, with properties
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
//!   ([`ServerView`]), gathered at one virtual time into [`Observations`]. The ledger also keeps what the
//!   agents sent and were sent for the whole-host handshake ([`ReadyReport`],
//!   [`StartSeen`], [`StopSeen`]).
//! - [`invariant`]: the oracle ([`Invariant`]), the [`Checker`] that tests it after each
//!   step and once at the end, and the one-line [`Violation`] it reports. Twenty-one
//!   properties: no host booked past what it holds, no lease on two hosts and no host
//!   listed twice ([`Invariant::NoDoubleBooking`]); each run launched at most once
//!   ([`Invariant::AtMostOneLaunch`]); one terminal answer per item
//!   ([`Invariant::OneTerminalAnswer`]); no new lease on a host the server lists as lost
//!   ([`Invariant::NoLeaseOnALostHost`]); everything settled once quiet
//!   ([`Invariant::SettlesWhenQuiet`]); no booking without a run and no run without a
//!   booking at the end ([`Invariant::BookingsMatchAgents`]); no result lost or
//!   recorded twice ([`Invariant::NoResultLost`]); and the four of a whole host: no host
//!   held by two whole-host leases or booked beside one
//!   ([`Invariant::WholeHostIsExclusive`]), a whole-host lease granted only after its
//!   host's agent reported ready for exactly that lease ([`Invariant::HeldMeansReady`]),
//!   no run on a host drained for a reservation stopped that its client did not ask to
//!   stop ([`Invariant::DrainKillsNothing`]), and no ordinary start on a host set aside
//!   for a whole-host request but backfill inside its tail
//!   ([`Invariant::MarkedHostsServeNoOrdinaryStart`]); and the five of a job: at most one
//!   allocation booked per job, a new id for each placement and no job on two hosts
//!   ([`Invariant::OneAllocationPerJob`]), a lost job never read in another state and a
//!   later listing of it answered with a stop ([`Invariant::LostIsTerminal`]), an
//!   allocation freed only on the host's listing or the provider's absence
//!   ([`Invariant::FreedOnlyWhenConfirmed`]), every job start naming an allocation
//!   committed before it ([`Invariant::NoStartWithoutAllocation`]), and every result kept
//!   by its agent until acknowledged and stored once
//!   ([`Invariant::ResultKeptUntilAcknowledged`]); and the five of an elastic pool's
//!   cloud: no more of the scheduler's machines running than the pool's `max_instances`
//!   ([`Invariant::InstancesWithinLimits`]), no release of a machine the scheduler did not
//!   launch ([`Invariant::OnlyOwnMachinesReleased`]), no acquire once the pool's brake
//!   has tripped until it is reset ([`Invariant::NoAcquireWhilePlanOnly`]), no acquire
//!   inside the throttle wait of a 429 ([`Invariant::ThrottleHonoured`]), and every
//!   launched machine released, gone or preempted once quiet, none left running idle and
//!   undrained ([`Invariant::LaunchedMachinesSettle`]).
//! - [`bus`]: what an agent and the server send each other, one HTTP call as a request
//!   and its answer ([`Msg`]), and how a trace writes it.
//! - [`agent`]: the agent node ([`AgentNode`]), behind the real wire, which writes what it
//!   launches and completes, and the verdicts it reads, into the cell's [`Ledger`] and counts what happened to it
//!   ([`Counters`]). Its agent is one of two kinds ([`AgentKind`]): the conformance
//!   suite's in-process fake, the scale agent a cell runs by the hundred, or the real
//!   agent lifecycle of `js-agent` over an in-memory runtime, for a handful of hosts.
//!   Either kind has the node's side of the whole-host handshake, which prepares, reports
//!   ready, refuses or stays silent as its [`Prepare`] switch says. A fake agent also
//!   speaks the per-job messages.
//! - [`server`]: the real lease server in process as a kernel node ([`ServerNode`]), built
//!   from a [`ServerSpec`] by the server's own command-line parser, on the kernel's clock,
//!   its state in the store `--store` names ([`Store`]: a SQLite file, or with the feature
//!   `postgres` a Postgres database of its own), stopped between two inputs
//!   ([`ServerNode::stop`]), started again over its own files and store with `--boot
//!   resume` ([`ServerNode::restart`]), and with a second server beside it on the same
//!   store ([`ServerNode::start_beside`]);
//!   with its job service too, its job store minting ids from the seed.
//! - [`elastic`]: an elastic pool ([`ElasticPool`]): its cloud ([`Cloud`]), the elastic
//!   fake on the cell's clock behind a recording wrapper the server's scaler is handed,
//!   which writes down each acquire, release and list with its arguments, its answer and
//!   its virtual time ([`CloudCall`]); the launch payload a machine's agent reads its join
//!   token and host id from ([`Payload`]); the seed's entropy the scaler names machines
//!   with ([`SeededEntropy`]); and what the invariants read of the cloud ([`CloudFacts`]).
//!   A machine the scaler launches boots into a fake agent the pool's boot time later,
//!   and one the cloud stops running has its agent killed in the same step.
//! - [`CellNode`]: the two kinds of node, as the kernel runs them.
//! - [`work`]: one unit of client work ([`Work`]): a run that lasts a fixed time, what it
//!   asks for (a whole host too, [`Work::whole_host`]), and when the client gives up on
//!   it.
//! - [`front`]: the client driver. A [`Front`] turns units of work into calls to the
//!   server and reports what it hears as [`ClientEvent`]s, and keeps every answer it was
//!   given raw ([`Answer`]); [`JobFront`] submits each unit as a job to the job service.
//!   The client is not a bus node: the run loop polls it at exact virtual times and its
//!   calls are never dropped.
//! - [`jobs`]: the job front, and what the cell observes of jobs: the server's job and
//!   allocation tables as it read them ([`JobRead`], [`AllocationRead`]), and what the
//!   per-job agents sent and read ([`JobBeat`], [`JobReply`], [`ResultMade`]).
//! - [`cell`]: the run loop ([`Cell`]), which steps the kernel, runs the actions scripted
//!   at virtual times, polls the client and checks the invariants after every step, with
//!   the changes a scenario makes (agents added, killed, killed for good and replaced,
//!   partitions, faults on the bus, a restart of the server, client calls, items
//!   submitted and cancelled), and the end of a scenario with its end check
//!   ([`Cell::settle`]).
//! - [`actions`]: what a scenario does at a virtual time, one [`Action`] per file.
//! - [`scenario`]: the builder ([`Scenario`]): the world, the work, the actions, what is
//!   expected ([`Expect`], at the end or at a virtual time) and the witnesses that show the
//!   situation happened, run over seeds. The smoke family (`tests/smoke.rs`) proves the machine works; the fault
//!   family (`tests/faults.rs`) runs agents that die and return or never return,
//!   partitions that heal, a lossy wire, a silent host, a stale start after its stop, a
//!   restart of the server and a forgotten host, each over sixteen seeds; the whole-host
//!   families (`tests/whole_host.rs`, `tests/whole_host_faults.rs`) run whole-host jobs; the
//!   elastic family (`tests/elastic.rs`) runs pools whose machines the scaler launches on
//!   demand and releases once idle, billed as EC2 and as Lambda bill; the store family
//!   (`tests/store.rs`) runs the server's state in its store across a restart, a stop in a
//!   burst of hellos, a second server on the store and a crash at the scaler's acquire,
//!   over SQLite and, with the feature `postgres`, over Postgres.
//! - [`agreement`]: one scenario run with every agent a fake and again with every agent
//!   real, and the client's outcome of each item compared ([`Disagreement`]): how the fake
//!   is held to the real agent.
//! - [`sweep`]: a scenario family over a range of seeds split into shards, each shard a run
//!   of the test binary `tests/sweep.rs`, which the nightly `sim-sweep` workflow runs; a
//!   failure is printed with the command that replays its seed.
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
//! reads the cell's clock, its lease table times its saves on that clock, its job store
//! stamps its rows with that clock and mints its ids from a script drawn from the seed,
//! and usage recording, which reads the system's clock, is off. Its session secrets are random;
//! the agents echo them, and no trace line holds one.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod actions;
pub mod agent;
pub mod agreement;
pub mod bus;
pub mod cell;
mod counters;
pub mod elastic;
pub mod front;
pub mod invariant;
pub mod jobs;
mod ledger;
mod node;
pub mod observe;
pub mod replay;
pub mod scenario;
pub mod server;
pub mod sweep;
mod text;
pub mod work;

pub use actions::Action;
pub use agent::{AgentKind, AgentNode, AgentSpec, Prepare, SIM_RUN_IMAGE};
pub use agreement::{Disagreement, ItemDifference, Outcome, Side, compare, differences};
pub use bus::{CallId, Msg, Route};
pub use cell::{Cell, SecondServer, Setup, StoreRead};
pub use counters::{Counter, Counters};
pub use elastic::{
    AcquireAnswer, Cloud, CloudCall, CloudEvent, CloudFacts, ElasticPool, Machine, OfferSpec,
    Payload, ReleaseAnswer, SeededEntropy,
};
pub use front::{Answer, CallKind, ClientPort, Front, FrontFactory};
pub use invariant::{Checked, Checker, Invariant, NotSettled, Touched, Violation};
pub use jobs::{
    AllocationRead, AllocationRow, JOB_PREFIX, JobBeat, JobFront, JobRead, JobReply, JobStartSeen,
    ResultMade,
};
pub use ledger::{Acked, Completion, Ledger};
pub use node::CellNode;
pub use observe::{
    AgentTruth, Ask, ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Killed, Launch,
    Observations, ReadyReport, RunId, Running, RunningJob, ServerHost, ServerView, StartSeen,
    StopCommand, StopSeen, Verdict,
};
pub use replay::{Failure, FailureReport, drive, run};
pub use scenario::{Expect, HostIs, Passed, Scenario, ScenarioFailure, SeedRun};
pub use server::{EPOCH_MS, JOBS_PATH, ServerNode, ServerSpec, Shape, Store, WHOLE_CLASS};
pub use work::Work;
