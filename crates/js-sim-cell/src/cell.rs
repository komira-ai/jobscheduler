//! The run loop: the kernel, the real server, the agents, the client, and a check after
//! every step.
//!
//! A [`Cell`] owns the [`Sim`] of [`CellNode`]s, the [`Checker`], the actions scripted at
//! virtual times ([`Cell::at`]) and the client driver ([`Front`]). [`Cell::run_until`]
//! repeats, in this order: it reads the time of the kernel's next event, of the next
//! scripted action and of the client's next poll ([`Front::next_poll`]); when the event
//! comes first, or at the same time, it applies the event with [`Sim::step`]; otherwise,
//! when the action comes first or at the same time as the poll, it moves the clock to the
//! action's time with [`Sim::run_until`] and runs the action; otherwise it moves the clock
//! to the poll's time and polls the client, which makes its calls to the server there and
//! then. The client is polled at most once per virtual millisecond. After every step,
//! every poll and every action that changed something it builds what changed into the
//! [`Observations`] (the agent the step fed, the events the client heard, the ledger
//! entries appended, the whole-host facts the agents wrote, the server's listing once per
//! virtual second and at every client call or poll) and calls [`Checker::step`]. It stops at the end time or at the first
//! [`Violation`], which every later call returns again.
//!
//! [`Cell::settle`] ends a scenario: it runs every scripted action, the ones actions
//! script too, then the cell for the time it is given, and makes the end check
//! ([`Checker::end`]).
//!
//! A killed agent process is a fault until it is replaced ([`Cell::kill`]); one killed
//! for good ([`Cell::kill_for_good`]) is a host gone, not a fault. A restart of the
//! server ([`Cell::restart_server`]) is a moment, not a fault; a restart the server
//! refuses leaves it down and is kept for [`Cell::take_refused`], which is how the run of
//! a seed reports it.
//!
//! # The trace
//!
//! The cell writes a trace of its own, family `js-sim-cell`, beside the kernel's: what
//! the kernel does not see (a process killed, killed for good or replaced, the server
//! restarted or its restart refused, the server stopped and what a read of its store then
//! found, a second server started or refused on its store, a host joined with a minted
//! token (module `store`), the bus's faults changed, a client call and its status, an item submitted or cancelled and each event the client
//! heard, with the item's number and never the ids it learned), what the server's listing says of each host when it
//! changes, each launch in the ledger, what the cloud of an elastic pool did to each
//! machine by its number (module `elastic`), and the violation that stopped the run. A line
//! names hosts, statuses and counts; it never holds a token, a bearer, a session secret,
//! a body or an id the server generated. The hash of a run ([`Cell::trace_hash`]) is the
//! hash of the cell's trace closed by the kernel's hash, so it covers both. With trace
//! lines kept ([`Setup::with_trace_lines`]), [`Cell::trace_lines`] gives both traces as
//! one, each line where it happened.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use js_agent_wire::{InstanceIdentity, PER_JOB_CAPABILITY};
use js_conformance::agent::Defect;
use js_lease_server::StartupError;
use js_sim::{Chance, Faults, NodeId, Partition, Sim, SimRng, Trace, TraceHash};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, IdError, Resources};

use crate::agent::{AgentKind, AgentNode, AgentSpec, Prepare};
use crate::bus::{SERVER, agent_name};
use crate::counters::{Counter, Counters};
use crate::elastic::Cloud;
use crate::front::{Answer, ClientPort, Front};
use crate::invariant::{Checker, Invariant, Touched, Violation};
use crate::jobs::JobFront;
use crate::jobs::read::Reader;
use crate::ledger::Ledger;
use crate::node::{Applied, CellNode};
use crate::observe::{ClientEvent, Heard, ItemId, ItemRecord, Killed, Observations};
use crate::server::{JOIN_TOKEN, ServerNode, ServerSpec};
use crate::text;
use crate::usage::UsageReadFault;
use crate::work::Work;

mod cloud_faults;
mod cordon;
mod elastic;
mod store;

pub use store::{SecondServer, StoreRead};

/// How long an agent waits between two hellos that were not welcomed (design section
/// 5.1: a one second base).
pub const HELLO_INTERVAL: Duration = Duration::from_secs(1);

/// How often the run loop re-reads the server's listing when no client call does.
pub const LISTING_EVERY: Duration = Duration::from_secs(1);

/// The least time between two polls of the client.
const POLL_GAP: Duration = Duration::from_millis(1);

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

/// The client driver: the front, the number of the next item, and the earliest time of
/// the next poll.
struct Client {
    front: Box<dyn Front>,
    next_item: u32,
    after: FarmTime,
}

impl Client {
    /// When the next poll is due: when the front asks, but not before `after`.
    fn due(&self) -> Option<FarmTime> {
        let asked = self.front.next_poll();
        asked.map(|at| at.max(self.after))
    }
}

/// The server as a poll of the client calls it: each call through the client router at
/// the poll's time, written down for the trace.
struct Recording<'a> {
    now: FarmTime,
    server: Option<&'a mut ServerNode>,
    lines: Vec<String>,
}

impl ClientPort for Recording<'_> {
    fn call(&mut self, request: Request<Body>) -> Response<String> {
        let method = request.method().as_str().to_owned();
        let now = self.now;
        let server = self.server.as_deref_mut();
        let answer = server.map_or_else(Response::default, |s| s.client_call(now, request));
        let status = answer.status().as_u16();
        self.lines.push(format!("client {method} {status}"));
        answer
    }
}

/// The real lease server and its agents under the kernel, checked after every step.
pub struct Cell {
    seed: u64,
    sim: Sim<CellNode>,
    checker: Checker,
    observations: Observations,
    ledger: Ledger,
    applied: Applied,
    capacity: Resources,
    spec: ServerSpec,
    /// The cell's directory, where each real agent makes its own.
    dir: PathBuf,
    agents: usize,
    /// The worker lag of the agents added from now on.
    worker_lag: Duration,
    script: BTreeMap<(FarmTime, u64), Action>,
    scripted: u64,
    client: Client,
    hook: Option<Hook>,
    checks: u64,
    listed_at: Option<FarmTime>,
    faulted: Faulted,
    violation: Option<Violation>,
    /// The first restart the server refused, until it is taken.
    refused: Option<StartupError>,
    trace: Trace,
    /// Both traces as one, when lines are kept, and how many kernel lines it holds.
    merged: Option<Vec<String>>,
    kernel_lines: usize,
    /// What the cell last read of the server's job tables.
    tables: Reader,
    /// What the cell knows of the machines of its elastic pool's cloud.
    machines: elastic::Machines,
    /// The join tokens the cell minted, and what it saw of the server's store.
    kept: store::Kept,
    /// The defect the cell applies to the usage rows it reads at the end.
    usage_fault: UsageReadFault,
}

impl Cell {
    /// A cell of `setup`'s seed: the server `spec` describes, its files in `dir` (and the
    /// directories of any real agent), with `faults` on the bus, no agent yet, and a
    /// [`JobFront`] as its client.
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
        let path = dir.path().to_path_buf();
        let server = ServerNode::start_seeded(&spec, dir, setup.seed)?;
        let mut sim = Sim::new(setup.seed, faults.clone());
        let mut trace = Trace::new("js-sim-cell");
        let mut merged = None;
        if setup.lines {
            sim = sim.with_trace_lines();
            trace.keep_lines();
            merged = Some(Vec::new());
        }
        sim.add_node(SERVER, CellNode::Server(Box::new(server)));
        let client = Client {
            front: Box::new(JobFront::new()),
            next_item: 0,
            after: FarmTime::default(),
        };
        let mut cell = Self {
            seed: setup.seed,
            sim,
            checker: Checker::new(),
            observations: Observations::quiet(),
            ledger: Ledger::new(),
            applied: Applied::default(),
            capacity,
            spec,
            dir: path,
            agents: 0,
            worker_lag: Duration::ZERO,
            script: BTreeMap::new(),
            scripted: 0,
            client,
            hook: None,
            checks: 0,
            listed_at: None,
            faulted: Faulted::default(),
            violation: None,
            refused: None,
            trace,
            merged,
            kernel_lines: 0,
            tables: Reader::default(),
            machines: elastic::Machines::default(),
            kept: store::Kept::default(),
            usage_fault: UsageReadFault::default(),
        };
        cell.set_faults(faults);
        Ok(cell)
    }

    /// Makes the agents added from now on take `lag` to act on a reply they read
    /// ([`crate::AgentSpec::worker_lag`]).
    pub fn set_worker_lag(&mut self, lag: Duration) {
        self.worker_lag = lag;
    }

    /// Adds the next agent, `h000` first, with a correct agent process.
    ///
    /// # Errors
    ///
    /// Never for fewer than a thousand million agents: the name is a host id.
    pub fn add_agent(&mut self) -> Result<HostId, IdError> {
        self.add(AgentKind::Fake(None))
    }

    /// Adds the next agent, whose agent lacks `defect`.
    ///
    /// # Errors
    ///
    /// As [`Cell::add_agent`].
    pub fn add_broken_agent(&mut self, defect: Defect) -> Result<HostId, IdError> {
        self.add(AgentKind::Fake(Some(defect)))
    }

    /// Adds the next agent, whose process is the real agent lifecycle over an in-memory
    /// runtime, with its directories in the cell's.
    ///
    /// # Errors
    ///
    /// As [`Cell::add_agent`].
    pub fn add_real_agent(&mut self) -> Result<HostId, IdError> {
        self.add(AgentKind::Real(self.dir.clone()))
    }

    fn add(&mut self, kind: AgentKind) -> Result<HostId, IdError> {
        self.add_as(kind, JOIN_TOKEN.to_owned(), None)
    }

    /// Adds the next agent, of `kind`, saying hello with `join_token` and naming `instance`.
    fn add_as(
        &mut self,
        kind: AgentKind,
        join_token: String,
        instance: Option<InstanceIdentity>,
    ) -> Result<HostId, IdError> {
        let name = agent_name(self.agents);
        let host = HostId::new(name.as_str())?;
        let mut capabilities = self.spec.shape().capabilities.clone();
        capabilities.insert(PER_JOB_CAPABILITY.to_owned(), "true".to_owned());
        let spec = AgentSpec {
            host: host.clone(),
            pool: self.spec.pool().to_owned(),
            join_token,
            capacity: self.capacity.clone(),
            capabilities,
            labels: BTreeMap::new(),
            hello_interval: HELLO_INTERVAL,
            worker_lag: self.worker_lag,
            kind,
            instance,
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
            let poll = self.client.due();
            let times = event.into_iter().chain(action).chain(poll);
            let next = times.fold(end, FarmTime::min);
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
            } else if poll == Some(next) {
                self.sim.run_until(next);
                self.poll();
            } else {
                self.sim.run_until(end);
                return Ok(());
            }
        }
    }

    /// Runs every scripted action, the ones an action scripts too, then the cell for
    /// `within` more of virtual time, and makes the end check: [`Checker::end`] over what
    /// the cell observes then, with the server's listing read again. The end of a fault
    /// (a partition healed, the bus's faults put back) is an action, so `within` is
    /// virtual time after the last fault ended, and after the last action of any kind.
    ///
    /// # Errors
    ///
    /// The first violation of a step or end invariant. A fault still active at the end
    /// is a violation of [`Invariant::SettlesWhenQuiet`]: the scenario never went quiet.
    pub fn settle(&mut self, within: Duration) -> Result<(), Violation> {
        while let Some(&(last, _)) = self.script.last_key_value().map(|(key, _)| key) {
            self.run_until(last)?;
        }
        let end = self.sim.now().saturating_add(within);
        self.run_until(end)?;
        self.reread(true);
        self.read_usage();
        self.read_cloud();
        self.observations.at = self.sim.now();
        let at = self.observations.at;
        let found = Checker::end(&self.observations, within).unwrap_or_else(|refused| {
            let invariant = Invariant::SettlesWhenQuiet;
            let detail = refused.to_string();
            vec![Violation {
                invariant,
                at,
                detail,
            }]
        });
        self.stop_at(found.into_iter().next());
        self.violation.clone().map_or(Ok(()), Err)
    }

    /// Makes `front` the cell's client, in place of the one it has. The old front is
    /// dropped with every item it held: an item submitted before is never polled again
    /// and never ends, so a scenario sets its front before it submits anything.
    pub fn set_front(&mut self, front: Box<dyn Front>) {
        self.client.front = front;
    }

    /// Submits one item of `work` now: the cell records what it asks for and that it was
    /// submitted, hands it to the client, and checks. Items are numbered from zero.
    pub fn submit(&mut self, work: &Work) -> ItemId {
        let now = self.sim.now();
        let item = ItemId(self.client.next_item);
        self.client.next_item += 1;
        let heard = vec![Heard {
            at: now,
            event: ClientEvent::Submitted,
        }];
        let record = ItemRecord {
            ask: work.ask(now),
            heard,
        };
        self.observations.items.insert(item, record);
        self.client.front.submit(item, work, now);
        let submitted = text::event(&ClientEvent::Submitted);
        self.record(format!("client {} {submitted}", text::item(item)));
        self.check(Touched::nothing().item(item));
        item
    }

    /// Asks the client to cancel `item`, now; the observations keep when the client
    /// first asked ([`Observations::cancels`]).
    pub fn cancel_item(&mut self, item: ItemId) {
        let now = self.sim.now();
        self.observations.cancels.entry(item).or_insert(now);
        self.client.front.cancel(item, now);
        self.record(format!("client {} cancel", text::item(item)));
    }

    /// The kernel's random source, from the seed: how a scenario draws a choice (which
    /// agents, when) so that it follows the seed.
    pub fn rng(&mut self) -> &mut SimRng {
        self.sim.rng()
    }

    /// The seed the cell runs.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// How many agents the cell has added.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents
    }

    /// Every agent's counts added together, and the elastic pool's cloud's
    /// ([`Cloud::counters`]).
    #[must_use]
    pub fn total_counters(&self) -> Counters {
        let agents = (0..self.agents).filter_map(|index| self.counters(index));
        let mut total = Counters::sum(agents);
        let cloud = self.cloud().map(Cloud::counters);
        cloud.iter().for_each(|counted| total.absorb(counted));
        total
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
            let at = self.sim.now();
            let kill = Killed {
                host: host.clone(),
                at,
            };
            self.observations.kills.push(kill);
            self.faulted.killed.insert(host.clone());
            self.fault_changed();
            self.check(Touched::nothing().agent(host));
        }
    }

    /// Kills the process of agent `index` for good, as [`Cell::kill`] does, with its host
    /// gone rather than faulted: it is never replaced, so the quiet the end check waits
    /// for starts now, as if the host had been taken out of the cell.
    pub fn kill_for_good(&mut self, index: usize) {
        self.kill(index);
        let host = self.agent(index).map(|agent| agent.host().clone());
        if let Some(host) = host {
            self.record(format!("gone {host}"));
            self.faulted.killed.remove(&host);
            self.fault_changed();
        }
    }

    /// Makes agent `index` answer a prepare as `prepare` says, from its next heartbeat on
    /// ([`AgentNode::set_prepare`]); an index that names no agent changes nothing.
    pub fn set_prepare(&mut self, index: usize, prepare: Prepare) {
        if let Some(agent) = self.agent_mut(index) {
            agent.set_prepare(prepare);
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

    /// Bumps [`Counter::CutWithUnlistedRun`] of agent `index` when its process runs a run
    /// or a job that no hello or heartbeat it sent listed ([`AgentNode::runs_unlisted`]): what a
    /// cut of the agent notes as it starts.
    pub(crate) fn note_cut(&mut self, index: usize) {
        let agent = self.agent_mut(index).filter(|agent| agent.runs_unlisted());
        if let Some(agent) = agent {
            agent.count(Counter::CutWithUnlistedRun);
        }
    }

    /// Stops the server and starts it again over the same directory with `--boot
    /// resume`, on the same clock ([`ServerNode::restart`]), keeping the agents and the
    /// client; the cell then reads the listing again and checks. A restart the server
    /// refuses leaves it down and is kept, the first one only, for
    /// [`Cell::take_refused`].
    pub fn restart_server(&mut self) {
        let spec = self.spec.clone();
        self.restart_server_with(&spec);
    }

    /// Restarts the server as [`Cell::restart_server`] does, with the flags `spec`
    /// describes in place of the cell's (an operator who restarts the server with a
    /// changed configuration). The cell's own spec, which the agents and the client were
    /// made for, stays as it is.
    pub fn restart_server_with(&mut self, spec: &ServerSpec) {
        let server = self.sim.nodes_mut().find_map(|(_, n)| n.as_server_mut());
        match server.map(|server| server.restart(spec)) {
            Some(Err(error)) => {
                self.record(format!("restart refused {}", error.flag()));
                self.refused = self.refused.take().or(Some(error));
            }
            _ => self.record("restart server".to_owned()),
        }
        self.reread(true);
        self.check(Touched::nothing().server());
    }

    /// The first restart the server refused, taken, so it is returned once.
    pub fn take_refused(&mut self) -> Option<StartupError> {
        self.refused.take()
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
        self.reread(true);
        self.check(Touched::nothing().server());
        answer
    }

    /// Every answer the client was given, in the order given ([`Front::answers`]).
    #[must_use]
    pub fn answers(&self) -> &[Answer] {
        self.client.front.answers()
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
            self.reread(false);
            touched = touched.server();
        }
        touched
    }

    fn agent_of(&self, host: &HostId) -> Option<&AgentNode> {
        let machine = self.machines.node(host).cloned();
        let node = machine.unwrap_or_else(|| NodeId::new(host.as_str()));
        self.sim.node(&node).and_then(CellNode::as_agent)
    }

    /// Reads the server's listing into the observations, and its job tables (always when
    /// `force`, else when a read of them is due), writing each host, job and allocation whose entry changed into
    /// the trace.
    fn reread(&mut self, force: bool) {
        let now = self.sim.now();
        let view = self.server().map(ServerNode::view).unwrap_or_default();
        let old = &self.observations.server;
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
        let server = self.sim.node(&NodeId::new(SERVER));
        let server = server.and_then(CellNode::as_server);
        let lines = self.tables.read(server, &mut self.observations, now, force);
        lines.into_iter().for_each(|line| self.record(line));
    }

    /// Polls the client now: its calls go to the server, each is written into the trace,
    /// then each event heard; the listing is read again and the cell checks.
    fn poll(&mut self) {
        let now = self.sim.now();
        self.client.after = now.saturating_add(POLL_GAP);
        let server = self.sim.nodes_mut().find_map(|(_, n)| n.as_server_mut());
        let mut port = Recording {
            now,
            server,
            lines: Vec::new(),
        };
        let heard = self.client.front.poll(now, &mut port);
        for line in port.lines {
            self.record(line);
        }
        let mut touched = Touched::nothing().server();
        for (item, event) in heard {
            self.record(format!(
                "client {} {}",
                text::item(item),
                text::event(&event)
            ));
            let record = self.observations.items.entry(item).or_default();
            record.heard.push(Heard { at: now, event });
            touched = touched.item(item);
        }
        self.reread(true);
        self.check(touched);
    }

    /// Checks the step invariants over what `touched` names, after reading the ledger.
    fn check(&mut self, touched: Touched) {
        self.read_cloud();
        self.observations.at = self.sim.now();
        let launches = self.ledger.launches_after(self.observations.ledger.len());
        for launch in &launches {
            self.record(format!("launch {}", launch.host));
        }
        self.observations.ledger.extend(launches);
        let seen = &self.observations;
        let counts = [seen.ready.len(), seen.starts.len(), seen.stops.len()];
        let (ready, starts, stops) = self.ledger.whole_after(counts);
        self.observations.ready.extend(ready);
        self.observations.starts.extend(starts);
        self.observations.stops.extend(stops);
        let ended = self.observations.completions.len();
        let completions = self.ledger.completions_after(ended);
        self.observations.completions.extend(completions);
        let judged = self.observations.verdicts.len();
        let verdicts = self.ledger.verdicts_after(judged);
        self.observations.verdicts.extend(verdicts);
        let seen = &self.observations;
        let counts = [
            seen.job_beats.len(),
            seen.job_replies.len(),
            seen.results_made.len(),
        ];
        let (beats, replies, made) = self.ledger.jobs_after(counts);
        self.observations.job_beats.extend(beats);
        self.observations.job_replies.extend(replies);
        self.observations.results_made.extend(made);
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
        self.stop_at(found.or(hooked));
    }

    /// Keeps `first` as the violation that stops the run, unless one stopped it before,
    /// and writes it into the trace.
    fn stop_at(&mut self, first: Option<Violation>) {
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
