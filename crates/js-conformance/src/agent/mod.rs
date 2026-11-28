//! The host-agent conformance suite: what any agent must do, whatever wire it speaks.
//!
//! This module puts a host agent under test, and it is wire-neutral: the scenarios speak only
//! to [`AgentHarness`], a small set of operations (deliver a Start, deliver a Cancel,
//! drop the connection, advance an injected clock, read what the agent reported). One
//! harness per transport implements the trait over that transport's messages; the
//! scenarios do not change.
//!
//! # Plugging a transport in
//!
//! Implement [`AgentHarness`] for a type that owns one agent and its connection, then call
//! [`run`] with a function that builds a fresh one:
//!
//! ```
//! use js_conformance::agent::{self, FakeAgent};
//!
//! let report = agent::run(FakeAgent::new);
//! assert!(report.failed().is_empty());
//! ```
//!
//! Every scenario gets a fresh harness, and one that tries a case per fresh host (each way a
//! host job ends, each post-job check) takes more. Time is the harness's: [`AgentHarness::advance`]
//! moves an injected clock and the agent reads that clock, never the wall clock, so a
//! scenario neither sleeps nor asserts how long anything took. A harness over a real
//! process must give the agent a clock it can drive (a flag the agent reads, a paused
//! runtime clock) and must return from `advance` only after the agent has acted on the
//! new time.
//!
//! # What the scenarios check, and the broken agent each fails on
//!
//! [`FakeAgent`] is the correct in-process agent. Each [`Defect`] is the same agent
//! missing exactly one behaviour, and the crate's tests assert that each scenario fails
//! on its broken agent and that no other scenario does, so no scenario passes vacuously.
//!
//! # What a transport adapter maps
//!
//! - **Start validity is relative.** `start(lease, valid_for, waited)` is a Start that
//!   may be acted on for `valid_for` ms (0 means unbounded) and that waited `waited` ms
//!   before the agent acted on it; it is dropped when `waited > valid_for`, so exactly at
//!   the limit it runs and one tick past it does not. The adapter arranges the wait the
//!   way its transport counts it: the legacy agent measures from receipt on the agent's
//!   own clock (so the adapter holds the Start while the clock advances), the worker
//!   wire from the send time of the heartbeat the Start names (so the adapter backdates
//!   that heartbeat).
//! - **The fence window comes from the transport.** `fence_window` is the transport's own
//!   value, or `None` when the agent has none. `SilenceKeepsRunningLeases` waits twice the
//!   window (a day when there is none) and asks that a running lease still runs. The
//!   js-agent adapter (`tests/js_agent_suite.rs`) reports the agent's default silence bound
//!   (`--silence-stop`), past which the agent starts nothing new.
//! - **A fenced start names the job and its allocation.** `start_fenced(job, allocation)`
//!   is a start whose fence is the pair; the agent must refuse one whose fence it retired
//!   and take the same job under another allocation. The js-agent adapter sends it as a job
//!   start (`JobStart`), and its other operations as run starts, so `start` keeps its
//!   validity bound.
//! - **`Observed.launches` and `never_started` are derived.** Where the agent emits an
//!   event for a skipped stale Start (the legacy agent does) read that; where it does
//!   not (the worker wire), derive them from the Results and refusals that reached the
//!   harness and from what the job runner was asked to launch.
//! - **Results repeat.** An unacknowledged result is repeated in every heartbeat, so
//!   `heartbeat` makes the agent send one and `results_sent` counts every transmission.
//!   [`AgentHarness::answer`] answers a result with any verdict the scheduler gives
//!   ([`Answer`]); `ack` is the accepting answer.
//! - **A flawed start is refused before it is launched.** [`AgentHarness::start_flawed`]
//!   sends a start with one [`Flaw`]; the adapter puts the flaw where its wire carries it
//!   (a rule the wire's own check refuses, or one the agent refuses when it admits the
//!   start), and reads either refusal into [`Observed::refused`].
//! - **The cleanup and the checks are the agent's runtime's.** [`AgentHarness::remove_fails`]
//!   makes the runtime fail every removal of a run's container, and
//!   [`AgentHarness::check_finds`] makes the post-job check find something after a run
//!   ([`HostCheck`]); [`Observed::cleanup`] is what the agent reported of a run's cleanup,
//!   in order ([`Cleanup`]).
//! - **A taint is a report of its own.** [`Observed::tainted`] is whether the agent holds
//!   its host tainted, and [`Observed::taint_reports`] counts the heartbeats that carried the
//!   taint; [`AgentHarness::ack_taint`] is the scheduler's acknowledgement of it.
//! - **A restart is the agent's process.** [`AgentHarness::restart`] ends the agent's process
//!   and starts it again over the same host (its runs, its state directory), which sends its
//!   first heartbeat and is answered with a reply that says nothing of a taint.
//!   [`AgentHarness::consent_to_host_jobs`] is the operator's consent to jobs that may change
//!   the host; an adapter may give it by restarting the agent with it.
//! - **A whole-node lease is held by the scheduler's replies.** [`AgentHarness::hold_whole`]
//!   holds the host whole for a lease, every later reply keeps holding it, and
//!   [`AgentHarness::whole_ready`] is the agent's answer whether the host is ready for it.
//!
//! # The real agent in one process
//!
//! [`memory`] is a container runtime that lives in memory, with the clock the lifecycle
//! reads: the real agent lifecycle of `js-agent` runs on it in one process, against the
//! registry in this crate's `tests/real_agent.rs` and as the real agent node of the
//! simulated cell (`js-sim-cell`).

mod fake;
pub mod memory;
mod scenarios;

use std::collections::{BTreeMap, BTreeSet};

pub use fake::{Defect, FENCE_WINDOW, FakeAgent};
pub use scenarios::Scenario;

use crate::Verdict;

/// Whether the agent received a message the harness sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The agent received it.
    Delivered,
    /// The connection was down, so the agent never saw it. A scheduler would resend.
    Lost,
}

/// The verdict the scheduler gives a result it was sent ([`AgentHarness::answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Answer {
    /// The result is taken.
    Accepted,
    /// The result was taken before.
    Duplicate,
    /// The result is of an allocation the scheduler no longer holds for the job.
    Stale,
    /// The scheduler holds no lease of that id.
    UnknownLease,
    /// The result does not match what the scheduler holds for the lease.
    Mismatch,
    /// A verdict the agent does not know, as from a newer scheduler: the agent cannot tell
    /// that the result was taken, so it keeps sending it.
    Unrecognised,
}

impl Answer {
    /// Every verdict that ends the result's resending: all but [`Answer::Unrecognised`].
    pub const KNOWN: [Answer; 5] = [
        Answer::Accepted,
        Answer::Duplicate,
        Answer::Stale,
        Answer::UnknownLease,
        Answer::Mismatch,
    ];
}

/// One rule of the start contract a start breaks ([`AgentHarness::start_flawed`]): the
/// agent refuses such a start before it launches anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Flaw {
    /// The start gives the job no wall time (a limit of 0).
    NoWallTime,
    /// The start sets a label under the scheduler's reserved prefix.
    ReservedLabel,
    /// A fenced start that names its job and no allocation.
    UnstatedFence,
    /// The start mounts a directory under a root the agent does not serve.
    MountOutsideRoots,
    /// The start asks for a GPU on an agent whose shape has none.
    GpuOnGpulessShape,
    /// A VM start on an agent that has no VM backend.
    VmWithoutBackend,
}

impl Flaw {
    /// Every flaw, in the order the scenario sends them.
    pub const ALL: [Flaw; 6] = [
        Flaw::NoWallTime,
        Flaw::ReservedLabel,
        Flaw::UnstatedFence,
        Flaw::MountOutsideRoots,
        Flaw::GpuOnGpulessShape,
        Flaw::VmWithoutBackend,
    ];
}

/// What the post-job check finds after a run ([`AgentHarness::check_finds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostCheck {
    /// A process of the job's user is left on the host after the job.
    LeftoverProcess,
    /// The check could not run (it could not read the host's processes).
    CannotRun,
}

/// One step of a run's cleanup the agent reported ([`Observed::cleanup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cleanup {
    /// The run's container (or VM, or process) is gone.
    ContainerGone,
    /// The run's scratch directory is gone.
    ScratchGone,
    /// The run's slot is free for another run.
    SlotFreed,
}

/// What the agent has reported to the scheduler side, as the harness saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Leases the agent lists as running.
    pub running: BTreeSet<String>,
    /// Leases the agent stopped because it heard nothing for the fence window (a correct
    /// agent stops none that runs).
    pub fenced: BTreeSet<String>,
    /// Leases whose Start had waited past its validity and was dropped.
    pub never_started: BTreeSet<String>,
    /// How many times the agent actually launched each lease's job.
    pub launches: BTreeMap<String, u32>,
    /// One entry per transmission of a lease's result that reached the harness.
    pub results_sent: Vec<String>,
    /// Leases whose start the agent refused: a refusal it reported, or a start its wire
    /// refused as malformed. A refused start launches nothing.
    pub refused: BTreeSet<String>,
    /// Leases whose result, as the agent holds it to send, carries what the job used.
    pub usage: BTreeSet<String>,
    /// The steps of each run's cleanup, in the order the agent reported them.
    pub cleanup: Vec<(String, Cleanup)>,
    /// Whether the agent holds its host tainted.
    pub tainted: bool,
    /// How many heartbeats that reached the harness carried the taint.
    pub taint_reports: u32,
    /// Leases the agent recorded a finding for: something for a person to look at.
    pub findings: BTreeSet<String>,
}

/// One agent behind one transport, driven by the scenarios.
///
/// Times are milliseconds on the injected clock, which starts at 0. An operation that
/// needs the connection answers [`Delivery::Lost`] while it is down and changes nothing.
pub trait AgentHarness {
    /// The silence after which the agent stops starting work by itself, at least 2, or
    /// `None` for an agent that has no such bound.
    fn fence_window(&self) -> Option<u64>;

    /// The injected clock now.
    fn now(&self) -> u64;

    /// Moves the injected clock forward and lets the agent act on the new time.
    fn advance(&mut self, millis: u64);

    /// Delivers a Start for `lease` that is valid for `valid_for` ms (0 is unbounded) and
    /// has waited `waited` ms before the agent acts on it. Counts as scheduler contact
    /// when delivered.
    fn start(&mut self, lease: &str, valid_for: u64, waited: u64) -> Delivery;

    /// Delivers a start of the job `job` whose fence is `job` with `allocation`, with no
    /// validity bound. Counts as scheduler contact when delivered. The job is named by
    /// `job` in [`Observed`] and in the other operations.
    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery;

    /// Delivers a Cancel for `lease`. Counts as scheduler contact when delivered.
    fn cancel(&mut self, lease: &str) -> Delivery;

    /// Acknowledges the result of `lease`: [`AgentHarness::answer`] with
    /// [`Answer::Accepted`].
    fn ack(&mut self, lease: &str) -> Delivery {
        self.answer(lease, Answer::Accepted)
    }

    /// Answers the result of `lease` with `answer`. Counts as scheduler contact when
    /// delivered.
    fn answer(&mut self, lease: &str, answer: Answer) -> Delivery;

    /// The job of the running `lease` ends on its own, producing a result, which the
    /// agent sends at its next heartbeat (here, at once if connected).
    fn complete(&mut self, lease: &str);

    /// The job of the running `lease` ends on its own with a non-zero status, producing a
    /// failed result, which the agent sends at once if connected.
    fn fail(&mut self, lease: &str);

    /// Delivers a start for `lease` that breaks the rule `flaw` names, with no validity
    /// bound. Counts as scheduler contact when delivered.
    fn start_flawed(&mut self, lease: &str, flaw: Flaw) -> Delivery;

    /// Every later removal of the container of `lease` fails.
    fn remove_fails(&mut self, lease: &str);

    /// The post-job check after `lease` finds what `check` names.
    fn check_finds(&mut self, lease: &str, check: HostCheck);

    /// The operator consents to jobs that may change the host, from now on.
    fn consent_to_host_jobs(&mut self);

    /// Delivers a start for `lease` whose job may change the host (it is not contained),
    /// with no validity bound. Counts as scheduler contact when delivered.
    fn start_host(&mut self, lease: &str) -> Delivery;

    /// Acknowledges the agent's report of its taint. Counts as scheduler contact when
    /// delivered.
    fn ack_taint(&mut self) -> Delivery;

    /// The agent's process ends and starts again over the same host, connected: it sends
    /// its first heartbeat, which the scheduler answers with nothing of a taint.
    fn restart(&mut self);

    /// Holds the host whole for `lease`: the reply that says so, and every reply after it,
    /// names the lease. Counts as scheduler contact when delivered.
    fn hold_whole(&mut self, lease: &str) -> Delivery;

    /// Whether the agent reports its host ready for the whole-node lease it is held for:
    /// nothing is held on it. A ready answer is kept for the lease, as the lease may then
    /// start runs. Asked while no lease holds it, the answer is `false`.
    fn whole_ready(&mut self) -> bool;

    /// Delivers a start for `lease` under the whole-node lease the host is held for, with
    /// no validity bound. Counts as scheduler contact when delivered.
    fn start_whole(&mut self, lease: &str) -> Delivery;

    /// The agent sends a heartbeat now, repeating every result not yet acknowledged.
    /// Nothing is sent while the connection is down.
    fn heartbeat(&mut self);

    /// The scheduler is heard from, as an answered heartbeat.
    fn contact(&mut self);

    /// Drops the connection.
    fn disconnect(&mut self);

    /// Connects again; the agent's first heartbeat on the new connection follows.
    fn reconnect(&mut self);

    /// What the agent has reported so far.
    fn observed(&self) -> Observed;
}

/// The verdicts of one run, in [`Scenario::ALL`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    /// Each scenario with its verdict.
    pub results: Vec<(Scenario, Verdict)>,
}

impl AgentReport {
    /// The scenarios that failed.
    pub fn failed(&self) -> Vec<Scenario> {
        let failed = self
            .results
            .iter()
            .filter(|(_, verdict)| matches!(verdict, Verdict::Failed(_)));
        failed.map(|(scenario, _)| *scenario).collect()
    }
}

/// Runs every scenario, each on a fresh harness from `make` (a scenario that needs a host
/// no earlier step touched, such as one per way a host job ends, takes another).
pub fn run<H: AgentHarness>(make: impl Fn() -> H) -> AgentReport {
    let results = Scenario::ALL
        .iter()
        .map(|scenario| (*scenario, scenario.run(&make)));
    AgentReport {
        results: results.collect(),
    }
}
