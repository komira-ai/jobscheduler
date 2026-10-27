//! The core-driver conformance suite: what the loop around the scheduler must do, whatever
//! carries its messages.
//!
//! [`js_sched::Scheduler`] is a pure state machine: it takes an input and returns effects
//! (commit this record, send this `Start`, answer these waiters). Everything that makes a
//! server correct and that the machine cannot see belongs to the loop that carries those
//! effects out: when an effect is released relative to the store write it depends on, what
//! survives a crash, which ids may be handed out twice, which heartbeats are fed. The
//! scheduler's own simulations have no store, so none of this is checked there. This
//! module states it as six scenarios against [`CoreHarness`], a narrow set of operations
//! (submit a job, register a worker's stream, send a heartbeat, report a result, advance
//! an injected clock, refuse the store's next writes, crash and restart, read what left the
//! core). The scenarios do not name a transport.
//!
//! ```
//! use js_conformance::core::{self, ReferenceCore};
//!
//! let report = core::run(ReferenceCore::new);
//! assert!(report.failed().is_empty());
//! ```
//!
//! # The honest limit
//!
//! [`ReferenceCore`] is a minimal loop over [`js_sched::Scheduler`] and the in-memory
//! [`js_state::fake::Fake`]. It is test code, and it is the executable spec of the suite
//! as `FakeAgent` is of the agent suite: it shows that each scenario can pass, and each
//! [`Defect`] (the same loop with one rule broken) shows that each can fail. It is not the
//! server's core. When the real core (`js-server-core`) plugs into [`CoreHarness`], the
//! suite decides: where the two disagree, the model is fixed or deleted, and the model must
//! not grow features to keep up with the real one. The harness names (`Stream`, `Released`)
//! are the test harness's; they are not a session type for a server to adopt.
//!
//! # What a harness adapter maps
//!
//! - **Time is injected.** [`CoreHarness::advance`] moves the farm clock and feeds a tick;
//!   nothing here sleeps or asserts how long anything took. The scenarios do read the
//!   scheduler's own grace ([`js_sched::fence::LEASE_GRACE`]) as a distance on that clock.
//! - **Effects are released in order, after their store write.** `Released` and `Answered`
//!   list what left the core, in the order it left. `Observed::stored_leases` is read from
//!   the store, in the order the store took the rows.
//! - **A refused write is the store failing, not a conflict.** `refuse_writes(n)` makes the
//!   next `n` writes of control records fail; the core must keep the effects behind them
//!   and write again. An adapter over a real store wraps it to fail on request.
//! - **A restart keeps the term.** `crash_and_restart` drops everything in memory and
//!   rebuilds from the store at the same leader term; workers register again. Lease ids
//!   `(term, seq)` must still never repeat, which is what the sequence blocks are for.
//! - **A stream is one registration.** `worker_up` returns the stream of a new session and
//!   replaces the worker's earlier one; `heartbeat` names the stream it arrived on.

mod model;
mod scenarios;

use std::time::Duration;

use js_types::sched::{LeaseId, OperationId, WaiterId};

pub use model::{Defect, ReferenceCore, UNPLACEABLE_JOB};
pub use scenarios::Scenario;

use crate::Verdict;

/// One registration of a worker with the core, as the harness numbers them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stream(u64);

/// A `Start` that left the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    /// The worker it was sent to.
    pub worker: String,
    /// The stream it was sent on: the worker's newest when it left.
    pub stream: Stream,
    /// The lease it starts.
    pub lease: LeaseId,
    /// The operation the lease runs.
    pub operation: OperationId,
}

/// An answer (a result, or a refusal) that left the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    /// The operation answered.
    pub operation: OperationId,
    /// The callers it was answered for.
    pub waiters: Vec<WaiterId>,
}

/// What the core has done to the outside, as the harness saw it. It survives a restart:
/// it is the outside world's record, not the core's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Every `Start` released, in order.
    pub released: Vec<Released>,
    /// Every answer released, in order.
    pub answered: Vec<Answered>,
    /// The leases the store holds now (a finished operation's lease is gone), in the
    /// order the store took them.
    pub stored_leases: Vec<LeaseId>,
}

/// One core behind one transport, driven by the scenarios.
///
/// Every operation returns after the core has acted on it, effects carried out as far as
/// the store allows.
pub trait CoreHarness {
    /// Submits `job` for the caller `waiter` and returns the operation it became. Two
    /// jobs with different numbers are different work; the same number joins.
    fn submit(&mut self, waiter: u64, job: u8) -> OperationId;

    /// Registers `worker` on a new stream, replacing its earlier one, and returns it. The
    /// worker is the same daemon process each time.
    fn worker_up(&mut self, worker: &str) -> Stream;

    /// A heartbeat of `worker` listing `running`, arriving on `stream`.
    fn heartbeat(&mut self, worker: &str, stream: Stream, running: &[LeaseId]);

    /// The job of `lease` (of `operation`) ends and the worker reports success.
    fn report(&mut self, operation: OperationId, lease: LeaseId);

    /// Moves the injected clock forward by `by` and ticks the core.
    fn advance(&mut self, by: Duration);

    /// Makes the store refuse the next `count` writes of control records.
    fn refuse_writes(&mut self, count: u32);

    /// The server process dies and a new one starts on the same store. Workers must
    /// register again.
    fn crash_and_restart(&mut self);

    /// What the core has done so far.
    fn observed(&self) -> Observed;
}

/// The verdicts of one run, in [`Scenario::ALL`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreReport {
    /// Each scenario with its verdict.
    pub results: Vec<(Scenario, Verdict)>,
}

impl CoreReport {
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
pub fn run<H: CoreHarness>(make: impl Fn() -> H) -> CoreReport {
    let results = Scenario::ALL
        .iter()
        .map(|scenario| (*scenario, scenario.run(&mut make())));
    CoreReport {
        results: results.collect(),
    }
}
