//! The run loop: the kernel, the real server, the agents, and a check after every step.
//!
//! A [`Cell`] owns the [`Sim`] of [`CellNode`]s, the [`Checker`] and the actions scripted
//! at virtual times ([`Cell::at`]). [`Cell::run_until`] repeats, in this order: it reads
//! the time of the kernel's next event and of the next scripted action; when the event
//! comes first, or at the same time, it applies the event with [`Sim::step`]; otherwise it
//! moves the clock to the action's time with [`Sim::run_until`] and runs the action. After
//! every step and every action that changed something it builds what changed into the
//! [`Observations`] (the agent the step fed, the ledger entries appended, the server's
//! listing once per virtual second and at every client call) and calls
//! [`Checker::step`]. It stops at the end time or at the first [`Violation`], which every
//! later call returns again.
//!
//! # The trace
//!
//! The cell writes a trace of its own, family `js-sim-cell`, beside the kernel's: what
//! the kernel does not see (a process killed or replaced, the bus's faults changed, a
//! client call and its status), what the server's listing says of each host when it
//! changes, each launch in the ledger, and the violation that stopped the run. A line
//! names hosts, statuses and counts; it never holds a token, a bearer, a session secret,
//! a body or an id the server generated. The hash of a run ([`Cell::trace_hash`]) is the
//! hash of the cell's trace closed by the kernel's hash, so it covers both. With trace
//! lines kept ([`Setup::with_trace_lines`]), [`Cell::trace_lines`] gives both traces as
//! one, each line where it happened.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use js_conformance::agent::Defect;
use js_lease_server::StartupError;
use js_sim::{Chance, Faults, NodeId, Partition, Sim, Trace, TraceHash};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, IdError, Resources};

use crate::agent::{AgentNode, AgentSpec};
use crate::bus::{SERVER, agent_name};
use crate::counters::Counters;
use crate::invariant::{Checker, Touched, Violation};
use crate::ledger::Ledger;
use crate::node::{Applied, CellNode};
use crate::observe::Observations;
use crate::server::{JOIN_TOKEN, ServerNode, ServerSpec};

/// How long an agent waits between two hellos that were not welcomed (design section
/// 5.1: a one second base).
pub const HELLO_INTERVAL: Duration = Duration::from_secs(1);

/// How often the run loop re-reads the server's listing when no client call does.
pub const LISTING_EVERY: Duration = Duration::from_secs(1);

/// A scripted action: a change to the cell at a virtual time.
type Action = Box<dyn FnOnce(&mut Cell)>;

/// An extra check the run loop calls after every step, with the number of the check.
type Hook = Box<dyn FnMut(u64, &Observations) -> Option<Violation>>;

/// How a cell is to run: its seed, and whether it keeps its trace as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    seed: u64,
    lines: bool,
}

impl Setup {
    /// A run of `seed` that keeps only its trace's hash.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { seed, lines: false }
    }

    /// This run, keeping the trace as text as well.
    #[must_use]
    pub fn with_trace_lines(mut self) -> Self {
        self.lines = true;
        self
    }

    /// The seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }
}

/// The faults the cell has put on the bus, the partition and the agent processes, for
/// [`Observations::fault_active`].
#[derive(Default)]
struct Faulted {
    lossy: bool,
    partitioned: bool,
    killed: BTreeSet<HostId>,
}

impl Faulted {
    fn active(&self) -> bool {
        self.lossy || self.partitioned || !self.killed.is_empty()
    }
}

/// The real lease server and its agents under the kernel, checked after every step.
pub struct Cell {
    sim: Sim<CellNode>,
    checker: Checker,
    observations: Observations,
    ledger: Ledger,
    applied: Applied,
    capacity: Resources,
    spec: ServerSpec,
    agents: usize,
    script: BTreeMap<(FarmTime, u64), Action>,
    scripted: u64,
    hook: Option<Hook>,
    checks: u64,
    listed_at: Option<FarmTime>,
    faulted: Faulted,
    violation: Option<Violation>,
    trace: Trace,
    /// Both traces as one, when lines are kept, and how many kernel lines it holds.
    merged: Option<Vec<String>>,
    kernel_lines: usize,
}

impl Cell {
    /// A cell of `setup`'s seed: the server `spec` describes, its files in `dir`, with
    /// `faults` on the bus and no agent yet.
    ///
    /// # Errors
    ///
    /// When the server does not start, or `spec`'s shape is not a valid host.
    pub fn new(
        setup: Setup,
        spec: ServerSpec,
        dir: TestDir,
        faults: Faults,
    ) -> Result<Self, StartupError> {
        let capacity = spec
            .shape()
            .resources()
            .map_err(StartupError::because("shape"))?;
        let server = ServerNode::start(&spec, dir)?;
        let mut sim = Sim::new(setup.seed, faults.clone());
        let mut trace = Trace::new("js-sim-cell");
        let mut merged = None;
        if setup.lines {
            sim = sim.with_trace_lines();
            trace.keep_lines();
            merged = Some(Vec::new());
        }
        sim.add_node(SERVER, CellNode::Server(Box::new(server)));
        let mut cell = Self {
            sim,
            checker: Checker::new(),
            observations: Observations::quiet(),
            ledger: Ledger::new(),
            applied: Applied::default(),
            capacity,
            spec,
            agents: 0,
            script: BTreeMap::new(),
            scripted: 0,
            hook: None,
            checks: 0,
            listed_at: None,
            faulted: Faulted::default(),
            violation: None,
            trace,
            merged,
            kernel_lines: 0,
        };
        cell.set_faults(faults);
        Ok(cell)
    }

    /// Adds the next agent, `h000` first, with a correct agent process.
    ///
    /// # Errors
    ///
    /// Never for fewer than a thousand million agents: the name is a host id.
    pub fn add_agent(&mut self) -> Result<HostId, IdError> {
        self.add(None)
    }

    /// Adds the next agent, whose agent lacks `defect`.
    ///
    /// # Errors
    ///
    /// As [`Cell::add_agent`].
    pub fn add_broken_agent(&mut self, defect: Defect) -> Result<HostId, IdError> {
        self.add(Some(defect))
    }

    fn add(&mut self, defect: Option<Defect>) -> Result<HostId, IdError> {
        let name = agent_name(self.agents);
        let host = HostId::new(name.as_str())?;
        let spec = AgentSpec {
            host: host.clone(),
            pool: self.spec.pool().to_owned(),
            join_token: JOIN_TOKEN.to_owned(),
            capacity: self.capacity.clone(),
            capabilities: self.spec.shape().capabilities.clone(),
            labels: BTreeMap::new(),
            hello_interval: HELLO_INTERVAL,
            defect,
        };
        let mut agent = AgentNode::new(spec, self.ledger.clone());
        agent.report_to(self.applied.clone());
        self.sim.add_node(name, CellNode::Agent(Box::new(agent)));
        self.agents += 1;
        Ok(host)
    }

    /// Runs `action` on the cell at virtual time `at` (or as soon as the loop runs, if
    /// `at` has passed), after every kernel event of that time. Actions of one time run in
    /// the order they were scripted.
    pub fn at(&mut self, at: FarmTime, action: impl FnOnce(&mut Cell) + 'static) {
        self.script.insert((at, self.scripted), Box::new(action));
        self.scripted += 1;
    }

    /// Gives the run loop one more check, called after every step with the number of the
    /// check (from 1) and the observations; a violation it returns stops the run as any
    /// other. A test hook: it is how a test makes a violation at a step of its choosing.
    pub fn check_also(
        &mut self,
        hook: impl FnMut(u64, &Observations) -> Option<Violation> + 'static,
    ) {
        self.hook = Some(Box::new(hook));
    }

    /// Runs the cell until virtual time `end`, or until an invariant fails.
    ///
    /// # Errors
    ///
    /// The first violation, now or in an earlier call.
    pub fn run_until(&mut self, end: FarmTime) -> Result<(), Violation> {
        loop {
            if let Some(violation) = &self.violation {
                return Err(violation.clone());
            }
            let event = self.sim.next_event_time();
            let action = self.script.first_key_value().map(|(&(at, _), _)| at);
            let next = event.into_iter().chain(action).fold(end, FarmTime::min);
            if event == Some(next) {
                self.sim.step();
                let touched = self.touched_by_step();
                self.check(touched);
            } else if let Some(first) = self.script.first_entry()
                && first.key().0 == next
            {
                let act = first.remove();
                self.sim.run_until(next);
                act(self);
            } else {
                self.sim.run_until(end);
                return Ok(());
            }
        }
    }

    /// Kills the process of agent `index`: it holds nothing and drops every input until
    /// [`Cell::replace`].
    pub fn kill(&mut self, index: usize) {
        let killed = self.agent_mut(index).map(|agent| {
            agent.kill();
            agent.host().clone()
        });
        if let Some(host) = killed {
            self.record(format!("kill {host}"));
            self.observations.agents.remove(&host);
            self.faulted.killed.insert(host.clone());
            self.fault_changed();
            self.check(Touched::nothing().agent(host));
        }
    }

    /// Starts a new process on agent `index`'s host, with empty state.
    pub fn replace(&mut self, index: usize) {
        let replaced = self.agent_mut(index).map(|agent| {
            agent.replace();
            (agent.host().clone(), agent.truth())
        });
        if let Some((host, truth)) = replaced {
            self.record(format!("replace {host}"));
            self.observations
                .agents
                .extend(truth.map(|truth| (host.clone(), truth)));
            self.faulted.killed.remove(&host);
            self.fault_changed();
            self.check(Touched::nothing().agent(host));
        }
    }

    /// Cuts each group of agents (by index) off from the server and from the other
    /// agents, now, in place of any partition in force.
    pub fn partition(&mut self, groups: &[&[usize]]) {
        let groups = groups
            .iter()
            .map(|group| group.iter().map(|&i| agent_name(i)));
        let partition = Partition::new(groups);
        self.faulted.partitioned = partition != Partition::none();
        self.sim.partition_at(self.sim.now(), partition);
        self.fault_changed();
    }

    /// Ends the partition in force, now.
    pub fn heal(&mut self) {
        self.partition(&[]);
    }

    /// Replaces the faults of the bus: a message sent from now on takes `faults`.
    ///
    /// # Panics
    ///
    /// If `faults.min_delay` exceeds `faults.max_delay`, as [`Sim::set_faults`].
    pub fn set_faults(&mut self, faults: Faults) {
        let kinds = [
            ("drop", faults.drop),
            ("duplicate", faults.duplicate),
            ("reorder", faults.reorder),
        ];
        let lossy = kinds
            .iter()
            .filter(|(_, chance)| *chance != Chance::never());
        let names: Vec<&str> = lossy.map(|(name, _)| *name).collect();
        let (min, max) = (faults.min_delay.as_millis(), faults.max_delay.as_millis());
        self.record(format!(
            "faults delay={min}..{max}ms {}",
            js_sim::text::list(&names, |n| (*n).to_owned())
        ));
        self.faulted.lossy = !names.is_empty();
        self.sim.set_faults(faults);
        self.fault_changed();
    }

    /// One call to the server's client router, now, run to its end; the cell then
    /// re-reads the server's listing and checks. `None` only for a cell without a server,
    /// which [`Cell::new`] never makes.
    pub fn client_call(&mut self, request: Request<Body>) -> Option<Response<String>> {
        let now = self.sim.now();
        let method = request.method().as_str().to_owned();
        let server = self
            .sim
            .nodes_mut()
            .find_map(|(_, node)| node.as_server_mut());
        let answer = server.map(|server| server.client_call(now, request));
        let status = answer.as_ref().map_or(0, |answer| answer.status().as_u16());
        self.record(format!("client {method} {status}"));
        self.reread();
        self.check(Touched::nothing().server());
        answer
    }

    /// What the cell observes now.
    #[must_use]
    pub fn observations(&self) -> &Observations {
        &self.observations
    }

    /// The cell's launch and completion ledger.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Agent `index`'s host.
    #[must_use]
    pub fn agent(&self, index: usize) -> Option<&AgentNode> {
        self.sim
            .node(&agent_name(index))
            .and_then(CellNode::as_agent)
    }

    /// What agent `index` counted.
    #[must_use]
    pub fn counters(&self, index: usize) -> Option<&Counters> {
        self.agent(index).map(AgentNode::counters)
    }

    /// The server.
    #[must_use]
    pub fn server(&self) -> Option<&ServerNode> {
        self.sim
            .node(&NodeId::new(SERVER))
            .and_then(CellNode::as_server)
    }

    /// The kernel.
    #[must_use]
    pub fn sim(&self) -> &Sim<CellNode> {
        &self.sim
    }

    /// The current virtual time.
    #[must_use]
    pub fn now(&self) -> FarmTime {
        self.sim.now()
    }

    /// How many checks the run loop has made.
    #[must_use]
    pub fn checks(&self) -> u64 {
        self.checks
    }

    /// The hash of the run so far: the cell's trace closed by the kernel's hash.
    #[must_use]
    pub fn trace_hash(&self) -> TraceHash {
        let mut trace = self.trace.clone();
        trace.record(&format!("kernel {}", self.sim.trace_hash()));
        trace.hash()
    }

    /// Both traces as one, in the order the lines happened; `None` unless the setup kept
    /// lines.
    pub fn trace_lines(&mut self) -> Option<&[String]> {
        self.sync_lines();
        self.merged.as_deref()
    }

    fn agent_mut(&mut self, index: usize) -> Option<&mut AgentNode> {
        let name = agent_name(index);
        let found = self.sim.nodes_mut().find(|(id, _)| **id == name);
        found.and_then(|(_, node)| node.as_agent_mut())
    }

    /// What the step just applied changed: the agent it fed, and the server's listing
    /// when it is due a re-read.
    fn touched_by_step(&mut self) -> Touched {
        let mut touched = Touched::nothing();
        if let Some(host) = self.applied.take() {
            let truth = self.agent_of(&host).and_then(AgentNode::truth);
            match truth {
                Some(truth) => self.observations.agents.insert(host.clone(), truth),
                None => self.observations.agents.remove(&host),
            };
            touched = touched.agent(host);
        }
        let due = self.listed_at.map(|at| at.saturating_add(LISTING_EVERY));
        if due.is_none_or(|due| due <= self.sim.now()) {
            self.reread();
            touched = touched.server();
        }
        touched
    }

    fn agent_of(&self, host: &HostId) -> Option<&AgentNode> {
        self.sim
            .node(&NodeId::new(host.as_str()))
            .and_then(CellNode::as_agent)
    }

    /// Reads the server's listing into the observations, writing each host whose entry
    /// changed into the trace.
    fn reread(&mut self) {
        let now = self.sim.now();
        let view = self.server().map(ServerNode::view).unwrap_or_default();
        let old = std::mem::take(&mut self.observations.server);
        let changed = view
            .hosts
            .iter()
            .filter(|(host, listed)| old.hosts.get(*host) != Some(*listed));
        let mut lines: Vec<String> = changed
            .map(|(host, listed)| {
                let state = if listed.lost { "lost" } else { "live" };
                let leases = listed.leases.len();
                format!(
                    "listing {host} {state} leases={leases} aside={}",
                    listed.set_aside
                )
            })
            .collect();
        let gone = old
            .hosts
            .keys()
            .filter(|host| !view.hosts.contains_key(*host));
        lines.extend(gone.map(|host| format!("listing {host} forgotten")));
        for line in lines {
            self.record(line);
        }
        self.observations.server = view;
        self.listed_at = Some(now);
    }

    /// Checks the step invariants over what `touched` names, after reading the ledger.
    fn check(&mut self, touched: Touched) {
        self.observations.at = self.sim.now();
        let launches = self.ledger.launches_after(self.observations.ledger.len());
        for launch in &launches {
            self.record(format!("launch {}", launch.host));
        }
        self.observations.ledger.extend(launches);
        self.checks += 1;
        let found = self
            .checker
            .step(&self.observations, &touched)
            .into_iter()
            .next();
        let checks = self.checks;
        let hooked = self
            .hook
            .as_mut()
            .and_then(|hook| hook(checks, &self.observations));
        let first = found.or(hooked);
        if let Some(violation) = &first {
            self.record(format!("violation {}", violation.invariant.name()));
        }
        self.violation = self.violation.take().or(first);
    }

    /// Notes whether a fault is now active; the end of the last one starts the quiet.
    fn fault_changed(&mut self) {
        let active = self.faulted.active();
        if self.observations.fault_active && !active {
            self.observations.quiet_since = self.sim.now();
        }
        self.observations.fault_active = active;
    }

    /// Writes one line of the cell's trace, at the current virtual time.
    fn record(&mut self, line: String) {
        let line = format!("{} cell {line}", self.sim.now().as_millis());
        self.sync_lines();
        self.trace.record(&line);
        if let Some(merged) = &mut self.merged {
            merged.push(line);
        }
    }

    /// Appends the kernel's lines written since the last call to the merged lines.
    fn sync_lines(&mut self) {
        let kernel = self.sim.trace_lines().unwrap_or_default();
        if let Some(merged) = &mut self.merged {
            merged.extend(kernel.iter().skip(self.kernel_lines).cloned());
            self.kernel_lines = kernel.len();
        }
    }
}
