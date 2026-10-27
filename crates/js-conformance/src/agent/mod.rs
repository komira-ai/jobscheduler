//! The host-agent conformance suite: what any agent must do, whatever wire it speaks.
//!
//! The provider suite of the crate root puts a [`js_provider::Provider`] under test. This
//! module puts a host agent under test, and it is wire-neutral: the scenarios speak only
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
//! The wire fields these scenarios read (a Start's validity bound, the fence window) are
//! the harness's to carry: a transport that has not got them yet cannot implement the
//! trait fully, which is the finding.

mod fake;
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
    /// Leases whose Start arrived past its validity bound and was dropped.
    pub never_started: BTreeSet<String>,
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
    /// The silence after which the agent stops its leases by itself. At least 2.
    fn fence_window(&self) -> u64;

    /// The injected clock now.
    fn now(&self) -> u64;

    /// Moves the injected clock forward and lets the agent act on the new time.
    fn advance(&mut self, millis: u64);

    /// Delivers a Start for `lease` that is valid until `not_after`. Counts as scheduler
    /// contact when delivered.
    fn start(&mut self, lease: &str, not_after: u64) -> Delivery;

    /// Delivers a Cancel for `lease`. Counts as scheduler contact when delivered.
    fn cancel(&mut self, lease: &str) -> Delivery;

    /// Acknowledges the result of `lease`. Counts as scheduler contact when delivered.
    fn ack(&mut self, lease: &str) -> Delivery;

    /// The job of the running `lease` ends on its own, producing a result.
    fn complete(&mut self, lease: &str);

    /// The scheduler is heard from, as an answered heartbeat.
    fn contact(&mut self);

    /// Drops the connection.
    fn disconnect(&mut self);

    /// Connects again, and lets the agent say what it has to say on a new connection.
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
