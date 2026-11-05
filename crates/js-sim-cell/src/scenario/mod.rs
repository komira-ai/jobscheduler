//! The scenario builder: a scenario is data plus one call, read top to bottom.
//!
//! ```text
//! Scenario::new("a_hundred_hosts_settle_four_hundred_runs")
//!     .scratch(env!("CARGO_TARGET_TMPDIR"))
//!     .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 100).heartbeat(secs(30), secs(90)))
//!     .hosts("burst", 100)
//!     .work(400, Work::run_for(secs(300)).give_up_after(secs(1800)))
//!     .at(secs(60), partition_agents(agents(0..10), secs(30)))
//!     .expect(Expect::all_items_terminal())
//!     .settle_within(secs(3600))
//!     .run_seeds(4)
//! ```
//!
//! A scenario states its world (the server, the hosts, the bus's faults at the start, the
//! client's [`FrontFactory`]), what happens and when ([`Scenario::work`] at time zero,
//! [`Action`]s at virtual times), and what it expects beyond the invariants ([`Expect`]
//! at the end, or at a virtual time with [`Scenario::expect_at`], and
//! [`Scenario::witness`]). Every invariant of [`crate::Invariant::ALL`] is checked in
//! every scenario: the step invariants after every step, the end invariants once
//! [`Scenario::settle_within`] after the last action ([`Cell::settle`]).
//!
//! [`Scenario::run_seeds`] runs seeds `0..n` in order, each in a cell of its own built
//! from the same data, and stops at the first failure: a violation, reported by
//! [`crate::FailureReport`] (the scenario, the seed, the trace hash, the invariant and the
//! last forty trace lines); an [`Expect`] a seed does not meet (the first one judged at a
//! virtual time that was not met, else the first judged at the end); or, once every seed
//! has run, a witness no seed satisfied.
//!
//! A witness is how a scenario shows that the situation it is about happened: a predicate
//! over the [`Counters`] of all a seed's agents added together, which at least one seed
//! must satisfy. A negative case (an agent that lacks a behaviour) is not vacuous when its
//! correct twin's witness holds.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use clap::ValueEnum;
use js_conformance::agent::Defect;
use js_lease_server::{Placement, StartupError};
use js_sim::text::list;
use js_sim::{Faults, TraceHash};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

use crate::actions::{Action, submit_work};
use crate::agreement::{self, Outcome};
use crate::cell::{Cell, Setup};
use crate::counters::{Counter, Counters};
use crate::front::{FrontFactory, LeaseFront};
use crate::observe::ItemId;
use crate::replay::{self, Failure, FailureReport, REPORT_LINES};
use crate::server::{ServerSpec, Shape};
use crate::work::Work;

mod answer;
mod expect;
mod facts;
mod launched_on;
mod tries;

pub use answer::Call;
pub(crate) use expect::ended;
pub use expect::{Expect, HostIs};
pub use facts::{Booked, HostRef, Listed, RecordFact};

/// How long a scenario runs after its last action unless [`Scenario::settle_within`]
/// says otherwise.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(600);

/// Which agent a group of hosts runs.
#[derive(Clone, Copy)]
enum Kind {
    /// The fake, lacking the defect when one is named.
    Fake(Option<Defect>),
    /// The real agent lifecycle.
    Real,
}

/// Hosts of one kind the scenario adds, in order.
#[derive(Clone)]
struct Hosts {
    pool: String,
    count: usize,
    kind: Kind,
}

/// What a seed's statements judged at a virtual time found unmet, in the order judged.
type Unmet = Rc<RefCell<Vec<String>>>;

/// A named predicate over a seed's counters.
#[derive(Clone)]
struct Witness {
    name: String,
    holds: Rc<dyn Fn(&Counters) -> bool>,
}

/// A scenario: its world, what happens and when, and what it expects.
#[derive(Clone)]
pub struct Scenario {
    name: String,
    scratch: Option<PathBuf>,
    server: ServerSpec,
    hosts: Vec<Hosts>,
    bus: Faults,
    actions: Vec<(FarmTime, Rc<dyn Action>)>,
    expects: Vec<Expect>,
    timed: Vec<(FarmTime, Expect)>,
    witnesses: Vec<Witness>,
    settle_within: Duration,
    front: FrontFactory,
}

/// One seed that passed.
#[derive(Clone, Debug)]
pub struct SeedRun {
    /// The seed.
    pub seed: u64,
    /// The hash of its run.
    pub hash: TraceHash,
    /// Its agents' counts added together.
    pub counters: Counters,
    /// What the client heard of each item by the end.
    pub outcomes: BTreeMap<ItemId, Outcome>,
}

/// Every seed of a scenario passed: the runs, in seed order.
#[derive(Clone, Debug)]
pub struct Passed {
    /// The seeds that ran, in order.
    pub runs: Vec<SeedRun>,
}

/// Why a scenario failed.
#[derive(Debug)]
pub enum ScenarioFailure {
    /// A seed's cell did not start.
    Start {
        /// The scenario.
        scenario: String,
        /// The seed.
        seed: u64,
        /// What the server, or the builder, refused.
        error: StartupError,
    },
    /// A seed broke an invariant.
    Violated(Box<FailureReport>),
    /// A seed did not meet an [`Expect`].
    Unmet {
        /// The scenario.
        scenario: String,
        /// The seed.
        seed: u64,
        /// The statement, what it wanted and what it got.
        detail: String,
    },
    /// No seed satisfied a witness.
    Unwitnessed {
        /// The scenario.
        scenario: String,
        /// The witness's name.
        witness: String,
        /// How many seeds ran.
        seeds: u64,
        /// What the agents counted, added over every seed.
        counted: Counters,
    },
}

/// One heading line, and for a violation the whole [`FailureReport`].
impl fmt::Display for ScenarioFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScenarioFailure::Start {
                scenario,
                seed,
                error,
            } => write!(f, "scenario {scenario} seed {seed} did not start: {error}"),
            ScenarioFailure::Violated(report) => write!(f, "{report}"),
            ScenarioFailure::Unmet {
                scenario,
                seed,
                detail,
            } => write!(f, "scenario {scenario} seed {seed} failed expect {detail}"),
            ScenarioFailure::Unwitnessed {
                scenario,
                witness,
                seeds,
                counted,
            } => {
                let counts = list(&Counter::ALL, |c| {
                    format!("{}={}", c.name(), counted.get(*c))
                });
                write!(
                    f,
                    "scenario {scenario}: witness {witness} held in none of {seeds} seeds; \
                     counted over them {counts}"
                )
            }
        }
    }
}

impl Scenario {
    /// A scenario named `name`, with no server, host, work or action yet: a server of no
    /// hosts, which refuses to start, until [`Scenario::server`].
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            scratch: None,
            server: ServerSpec::registered_pool("pool", Shape::gpu_host(), 0),
            hosts: Vec::new(),
            bus: Faults::default(),
            actions: Vec::new(),
            expects: Vec::new(),
            timed: Vec::new(),
            witnesses: Vec::new(),
            settle_within: DEFAULT_SETTLE,
            front: LeaseFront::factory(),
        }
    }

    /// This scenario, keeping each seed's server files in a directory of its own under
    /// `root` (a test passes `env!("CARGO_TARGET_TMPDIR")`), removed when the seed ends.
    #[must_use]
    pub fn scratch(mut self, root: impl Into<PathBuf>) -> Self {
        self.scratch = Some(root.into());
        self
    }

    /// This scenario, running the server `spec` describes.
    #[must_use]
    pub fn server(mut self, spec: ServerSpec) -> Self {
        self.server = spec;
        self
    }

    /// This scenario under the server's `placement` (`--placement`, as
    /// [`ServerSpec::placement`] sets it), its name followed by `-` and the placement as
    /// the server's command line spells it, so that the runs of one scenario under each
    /// placement are named apart in a failure and in the golden file. After
    /// [`Scenario::server`], which would put the default back.
    #[must_use]
    pub fn under(mut self, placement: Placement) -> Self {
        let spelled = placement.to_possible_value();
        let spelled = spelled.map(|value| value.get_name().to_owned());
        self.name = format!("{}-{}", self.name, spelled.unwrap_or_default());
        self.server = self.server.placement(placement);
        self
    }

    /// This scenario, with `count` more hosts of `pool` with correct fake agents: the
    /// scale agent.
    #[must_use]
    pub fn hosts(self, pool: &str, count: usize) -> Self {
        self.add_hosts(pool, count, Kind::Fake(None))
    }

    /// This scenario, with `count` more hosts of `pool` whose fake agents lack `defect`.
    #[must_use]
    pub fn broken_hosts(self, pool: &str, count: usize, defect: Defect) -> Self {
        self.add_hosts(pool, count, Kind::Fake(Some(defect)))
    }

    /// This scenario, with `count` more hosts of `pool` whose agents are the real agent
    /// lifecycle over an in-memory runtime. For a handful of hosts: each writes its held
    /// list into a directory of its own at every step.
    #[must_use]
    pub fn real_hosts(self, pool: &str, count: usize) -> Self {
        self.add_hosts(pool, count, Kind::Real)
    }

    fn add_hosts(mut self, pool: &str, count: usize, kind: Kind) -> Self {
        let pool = pool.to_owned();
        self.hosts.push(Hosts { pool, count, kind });
        self
    }

    /// This scenario, with `faults` on the agent wire from the start. A drop, duplicate
    /// or reorder chance is a fault, so a scenario that has one puts faults without one
    /// back with an action before it can settle.
    #[must_use]
    pub fn bus(mut self, faults: Faults) -> Self {
        self.bus = faults;
        self
    }

    /// This scenario, submitting `count` items of `work` at time zero.
    #[must_use]
    pub fn work(self, count: u32, work: Work) -> Self {
        self.at(Duration::ZERO, submit_work(count, work))
    }

    /// This scenario, running `action` `after` the start. Actions of one time run in the
    /// order they were added, after the kernel's events of that time.
    #[must_use]
    pub fn at(mut self, after: Duration, action: impl Action + 'static) -> Self {
        let at = FarmTime::default().saturating_add(after);
        self.actions.push((at, Rc::new(action)));
        self
    }

    /// This scenario, expecting `expect` of every seed.
    #[must_use]
    pub fn expect(mut self, expect: Expect) -> Self {
        self.expects.push(expect);
        self
    }

    /// This scenario, expecting `expect` of every seed at `after` the start: judged at that
    /// virtual time, after the kernel's events of that time and every action scripted
    /// for it, and before the client's calls of that time. A statement about the server
    /// reads it then; one about what the cell observed reads what it had observed by then.
    #[must_use]
    pub fn expect_at(mut self, after: Duration, expect: Expect) -> Self {
        let at = FarmTime::default().saturating_add(after);
        self.timed.push((at, expect));
        self
    }

    /// This scenario, failing unless at least one seed's agents' counts, added together,
    /// satisfy `holds`.
    #[must_use]
    pub fn witness(mut self, name: &str, holds: impl Fn(&Counters) -> bool + 'static) -> Self {
        self.witnesses.push(Witness {
            name: name.to_owned(),
            holds: Rc::new(holds),
        });
        self
    }

    /// This scenario, running `within` of virtual time after its last action (the end of
    /// its last fault is an action) before the end check.
    #[must_use]
    pub fn settle_within(mut self, within: Duration) -> Self {
        self.settle_within = within;
        self
    }

    /// This scenario, its client made by `front` in place of [`LeaseFront::factory`].
    #[must_use]
    pub fn front(mut self, front: FrontFactory) -> Self {
        self.front = front;
        self
    }

    /// Runs seeds `0..seeds` in order.
    ///
    /// # Panics
    ///
    /// With the [`ScenarioFailure`] as text, at the first failure: how a scenario fails
    /// its test.
    pub fn run_seeds(&self, seeds: u64) -> Passed {
        self.try_run_seeds(seeds)
            .unwrap_or_else(|failure| panic!("{failure}"))
    }

    /// Runs seeds `0..seeds` in order, stopping at the first that fails, then judges the
    /// witnesses over every seed.
    ///
    /// # Errors
    ///
    /// The first failure.
    pub fn try_run_seeds(&self, seeds: u64) -> Result<Passed, ScenarioFailure> {
        self.try_run_seed_range(0..seeds)
    }

    /// Runs the seeds of `seeds` in order, stopping at the first that fails, then judges
    /// the witnesses over every seed of the range: [`Scenario::try_run_seeds`] over a part
    /// of the seeds. A scenario is built and run on one thread, so a test that runs a
    /// large scenario's seeds on several threads builds the scenario on each and gives
    /// each a range.
    ///
    /// # Errors
    ///
    /// The first failure.
    pub fn try_run_seed_range(&self, seeds: Range<u64>) -> Result<Passed, ScenarioFailure> {
        let count = seeds.end.saturating_sub(seeds.start);
        let runs = seeds.map(|seed| self.run_seed(seed));
        let runs = runs.collect::<Result<Vec<SeedRun>, ScenarioFailure>>()?;
        let held = |witness: &&Witness| runs.iter().any(|run| (witness.holds)(&run.counters));
        let unwitnessed = self.witnesses.iter().find(|witness| !held(witness));
        match unwitnessed {
            Some(witness) => Err(ScenarioFailure::Unwitnessed {
                scenario: self.name.clone(),
                witness: witness.name.clone(),
                seeds: count,
                counted: Counters::sum(runs.iter().map(|run| &run.counters)),
            }),
            None => Ok(Passed { runs }),
        }
    }

    fn run_seed(&self, seed: u64) -> Result<SeedRun, ScenarioFailure> {
        let timed = Unmet::default();
        let build = |setup: Setup| self.build(setup, &timed);
        let settle = |cell: &mut Cell| cell.settle(self.settle_within);
        let ran = replay::drive(&self.name, seed, &build, &settle);
        let mut cell = ran.map_err(|failure| self.failed(seed, failure))?;
        let first = timed.borrow().first().cloned();
        let at_end = |cell: &mut Cell| self.expects.iter().find_map(|e| e.unmet(cell));
        let unmet = first.or_else(|| at_end(&mut cell));
        match unmet {
            Some(detail) => Err(ScenarioFailure::Unmet {
                scenario: self.name.clone(),
                seed,
                detail,
            }),
            None => Ok(SeedRun {
                seed,
                hash: cell.trace_hash(),
                counters: cell.total_counters(),
                outcomes: agreement::outcomes(cell.observations()),
            }),
        }
    }

    fn failed(&self, seed: u64, failure: Failure) -> ScenarioFailure {
        match failure {
            Failure::Start(error) => ScenarioFailure::Start {
                scenario: self.name.clone(),
                seed,
                error,
            },
            Failure::Violated(report) => ScenarioFailure::Violated(report),
        }
    }

    /// The scenario's name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The last [`REPORT_LINES`] lines of `seed`'s trace, from the seed run again to its
    /// end with its trace kept (whether it passes or not); none when its cell does not
    /// start.
    pub(crate) fn last_lines(&self, seed: u64) -> Vec<String> {
        let built = self.build(Setup::new(seed).with_trace_lines(), &Unmet::default());
        let lines = built.map(|mut cell| {
            let _ = cell.settle(self.settle_within);
            let lines = cell.trace_lines().unwrap_or_default();
            let start = lines.len().saturating_sub(REPORT_LINES);
            lines[start..].to_vec()
        });
        lines.unwrap_or_default()
    }

    /// The cell of one seed: the server, the front, the hosts in order, the actions, and
    /// the statements judged at a virtual time, each writing what it finds unmet into
    /// `unmet`.
    fn build(&self, setup: Setup, unmet: &Unmet) -> Result<Cell, StartupError> {
        let unnamed = StartupError::new("scratch", "the scenario names no scratch directory");
        let root = self.scratch.as_ref().ok_or(unnamed)?;
        let name = format!("{}-{}", self.name, setup.seed());
        let dir = TestDir::under(root.join("js-sim-cell"), &name);
        let mut cell = Cell::new(setup, self.server.clone(), dir, self.bus.clone())?;
        cell.set_front(self.front.make(&self.server));
        for hosts in &self.hosts {
            self.add(&mut cell, hosts)?;
        }
        for (at, action) in &self.actions {
            let action = Rc::clone(action);
            cell.at(*at, move |cell| action.run(cell));
        }
        for (at, expect) in &self.timed {
            let (at, expect, unmet) = (*at, expect.clone(), Rc::clone(unmet));
            cell.at(at, move |cell| {
                let found = expect.unmet(cell);
                let found = found.map(|detail| format!("at {} ms {detail}", at.as_millis()));
                unmet.borrow_mut().extend(found);
            });
        }
        Ok(cell)
    }

    /// Adds `hosts` to `cell`, refusing hosts of a pool the server does not have.
    fn add(&self, cell: &mut Cell, hosts: &Hosts) -> Result<(), StartupError> {
        if hosts.pool != self.server.pool() {
            let why = format!("the server has no pool {}", hosts.pool);
            return Err(StartupError::new("hosts", why));
        }
        for _ in 0..hosts.count {
            let added = match hosts.kind {
                Kind::Fake(Some(defect)) => cell.add_broken_agent(defect),
                Kind::Fake(None) => cell.add_agent(),
                Kind::Real => cell.add_real_agent(),
            };
            added.map_err(StartupError::because("hosts"))?;
        }
        Ok(())
    }
}
