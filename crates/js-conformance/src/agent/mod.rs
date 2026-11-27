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
//! Every scenario gets a fresh harness. Time is the harness's: [`AgentHarness::advance`]
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
//! - **The fence window comes from the transport.** `fence_window` is the transport's own
//!   value, or `None` when the agent has none. An agent with no self-fence fails
//!   `SelfFencesAfterSilence` with that reason rather than having it skipped. The js-agent
//!   adapter (`tests/js_agent_suite.rs`) reports the agent's default silence bound
//!   (`--silence-stop`).
//! - **A fenced start names the job and its allocation.** `start_fenced(job, allocation)`
//!   is a start whose fence is the pair; the agent must refuse one whose fence it retired
//!   and take the same job under another allocation. The js-agent adapter sends it as a job
//!   start (`JobStart`), and `start` as the start the agent makes of a run spec
//!   (`Lifecycle::start_spec`), which is keyed by job all the same.
//! - **`Observed.launches` is derived.** Derive it from the results and refusals that
//!   reached the harness and from what the job runner was asked to launch.
//! - **Results repeat.** An unacknowledged result is repeated in every heartbeat, so
//!   `heartbeat` makes the agent send one and `results_sent` counts every transmission.
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

/// What the agent has reported to the scheduler side, as the harness saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Leases the agent lists as running.
    pub running: BTreeSet<String>,
    /// Leases the agent stopped because it heard nothing for the fence window.
    pub fenced: BTreeSet<String>,
    /// How many times the agent actually launched each lease's job.
    pub launches: BTreeMap<String, u32>,
    /// One entry per transmission of a lease's result that reached the harness.
    pub results_sent: Vec<String>,
}

/// One agent behind one transport, driven by the scenarios.
///
/// Times are milliseconds on the injected clock, which starts at 0. An operation that
/// needs the connection answers [`Delivery::Lost`] while it is down and changes nothing.
pub trait AgentHarness {
    /// The silence after which the agent stops its leases by itself, at least 2, or
    /// `None` for an agent that has no self-fence.
    fn fence_window(&self) -> Option<u64>;

    /// The injected clock now.
    fn now(&self) -> u64;

    /// Moves the injected clock forward and lets the agent act on the new time.
    fn advance(&mut self, millis: u64);

    /// Delivers a Start for `lease`, whose fence is `lease` with an allocation of the
    /// harness's choosing. Counts as scheduler contact when delivered.
    fn start(&mut self, lease: &str) -> Delivery;

    /// Delivers a start of the job `job` whose fence is `job` with `allocation`. Counts as scheduler contact when delivered. The job is named by
    /// `job` in [`Observed`] and in the other operations.
    fn start_fenced(&mut self, job: &str, allocation: &str) -> Delivery;

    /// Delivers a Cancel for `lease`. Counts as scheduler contact when delivered.
    fn cancel(&mut self, lease: &str) -> Delivery;

    /// Acknowledges the result of `lease`. Counts as scheduler contact when delivered.
    fn ack(&mut self, lease: &str) -> Delivery;

    /// The job of the running `lease` ends on its own, producing a result, which the
    /// agent sends at its next heartbeat (here, at once if connected).
    fn complete(&mut self, lease: &str);

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

/// Runs every scenario, each on a fresh harness from `make`.
pub fn run<H: AgentHarness>(make: impl Fn() -> H) -> AgentReport {
    let results = Scenario::ALL
        .iter()
        .map(|scenario| (*scenario, scenario.run(&mut make())));
    AgentReport {
        results: results.collect(),
    }
}
