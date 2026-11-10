//! The run loop's side of an elastic pool (see the module doc of [`crate::elastic`]): after
//! every step it reads the machines each cloud launched, boots an agent for each the pool's
//! boot time later, kills the agent of each the cloud no longer runs, and refreshes what
//! the invariants read of each cloud ([`CloudFacts`]).
//!
//! The trace names a machine by its number, `m<n>`, in its cloud's launch order: `cloud
//! launch m<n>` when the cell first sees the cloud run it, `cloud boot m<n>` when its agent
//! starts (`cloud boot m<n> silent` for a machine that does not join, and nothing for a
//! machine the cloud no longer runs by then), and `cloud release m<n>`, `cloud preempt m<n>`
//! or `cloud drop m<n>` in the step the cloud stops running it, by the cloud's release, its
//! preemption, or the machine's disappearance; `cloud brake reset <bool>` for the
//! operator's reset of the pool's brake; and `cloud crash server` when an acquire crashed the
//! server ([`Cloud::crash_at_next_acquire`]). A cloud of a server of
//! [`crate::ServerSpec::ordered`] is named: its lines put its name after `cloud` (`cloud
//! <provider> launch m<n>`), and its machines' nodes are `<provider>-m<n>`. No line holds a
//! payload, a token, a host's name or an id the server or the cloud generated.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::{InstanceIdentity, PER_JOB_CAPABILITY};
use js_elastic::{Held, HostRef};
use js_sim::NodeId;
use js_types::HostId;

use super::{Cell, HELLO_INTERVAL};
use crate::agent::{AgentKind, AgentNode, AgentSpec};
use crate::elastic::{Cloud, CloudEvent, CloudFacts, Machine, Payload};
use crate::invariant::Touched;
use crate::node::CellNode;
use crate::observe::Killed;
use crate::server::ServerNode;

/// What the cell knows of the machines of its elastic pool's clouds.
#[derive(Default)]
pub(super) struct Machines {
    /// What the cell read of each cloud, in the server's order of them.
    clouds: Vec<Read>,
    /// The node of each launched machine's host.
    nodes: BTreeMap<HostId, NodeId>,
}

/// What the cell read of one cloud.
#[derive(Default)]
struct Read {
    /// How many of the cloud's machines (in its launch order) the cell has read.
    read: usize,
    /// How many of the cloud's calls the observations hold.
    calls: usize,
    /// How many of what its wrapper saw at an acquire the observations hold.
    seen: usize,
    /// Each machine the cloud launched for the scheduler, by number.
    launched: Vec<Launched>,
}

impl Machines {
    /// The node of `host`'s agent, when `host` is a launched machine's.
    pub(super) fn node(&self, host: &HostId) -> Option<&NodeId> {
        self.nodes.get(host)
    }
}

/// One machine the cloud launched for the scheduler.
struct Launched {
    /// The cloud's id of it.
    id: String,
    /// What its launch payload holds.
    payload: Payload,
    /// The host id its agent registers under.
    host: HostId,
    /// The acquire id it was launched for.
    acquire: String,
    /// Whether its agent was started.
    booted: bool,
    /// Whether the cloud no longer runs it.
    gone: bool,
}

/// How the cloud stopped running a machine.
fn ending(cloud: &Cloud, id: &str, known: &BTreeSet<String>) -> &'static str {
    let released = cloud.fake().released();
    let released = released.iter().any(|machine| machine.as_str() == id);
    match (released, known.contains(id)) {
        (true, _) => "release",
        (false, true) => "preempt",
        (false, false) => "drop",
    }
}

/// The node name of machine `number` of `cloud`: `m000`, `m001`, ..., after the cloud's
/// name and a dash for a named cloud.
fn node_name(cloud: Option<&str>, number: usize) -> NodeId {
    match cloud {
        Some(name) => NodeId::new(format!("{name}-m{number:03}")),
        None => NodeId::new(format!("m{number:03}")),
    }
}

/// The trace line of `what` happening to machine `number` of `cloud`.
fn line(cloud: Option<&str>, what: &str, number: usize) -> String {
    match cloud {
        Some(name) => format!("cloud {name} {what} m{number:03}"),
        None => format!("cloud {what} m{number:03}"),
    }
}

impl Cell {
    /// The elastic pool's cloud, for a cell of [`crate::ServerSpec::elastic_pool`]; the
    /// first launching provider's, for a cell of [`crate::ServerSpec::ordered`].
    #[must_use]
    pub fn cloud(&self) -> Option<&Cloud> {
        self.clouds().first()
    }

    /// Every cloud of the cell's server, first to last; none without an elastic pool.
    #[must_use]
    pub fn clouds(&self) -> &[Cloud] {
        self.server().map(ServerNode::clouds).unwrap_or_default()
    }

    /// The cloud whose pool is named `name`: the elastic pool's, or the machine provider of
    /// that name of a cell of [`crate::ServerSpec::ordered`].
    #[must_use]
    pub fn cloud_named(&self, name: &str) -> Option<&Cloud> {
        self.clouds()
            .iter()
            .find(|cloud| cloud.pool().name() == name)
    }

    /// The agent node of the first cloud's machine `number` (in launch order, from 0), once
    /// it has booted.
    #[must_use]
    pub fn machine(&self, number: usize) -> Option<&AgentNode> {
        let name = self.cloud().and_then(Cloud::named);
        self.sim
            .node(&node_name(name, number))
            .and_then(CellNode::as_agent)
    }

    /// Asks the server's scaler to reset the elastic pool's brake, as an operator does
    /// (applied at its next tick), and checks; `false` for a cell without an elastic pool.
    pub fn reset_brake(&mut self) -> bool {
        let reset = self.server().is_some_and(|server| server.reset_brake());
        self.record(format!("cloud brake reset {reset}"));
        self.check(Touched::nothing());
        reset
    }

    /// Makes the first cloud drop machine `number`, outside the scheduler (the machine
    /// vanishes, and a read of it says so), and checks: its agent is killed in this step.
    pub fn drop_machine(&mut self, number: usize) {
        self.on_machine(0, number, |cloud, id| cloud.fake().drop_machine(id));
    }

    /// Makes the first cloud preempt machine `number` (it stops, and reads preempted until
    /// it is released), and checks: its agent is killed in this step.
    pub fn preempt_machine(&mut self, number: usize) {
        self.on_machine(0, number, Cloud::preempt);
    }

    /// Makes the cloud named `provider` ([`Cell::cloud_named`]) preempt its machine
    /// `number`, as [`Cell::preempt_machine`], and checks. A provider the cell has no cloud
    /// of changes nothing.
    pub fn preempt_machine_of(&mut self, provider: &str, number: usize) {
        let mut clouds = self.clouds().iter();
        let at = clouds.position(|cloud| cloud.pool().name() == provider);
        self.on_machine(at.unwrap_or(usize::MAX), number, Cloud::preempt);
    }

    /// Acts on machine `number` of cloud `at` (in the server's order), when there is one,
    /// and checks.
    fn on_machine(&mut self, at: usize, number: usize, act: impl FnOnce(&Cloud, &HostRef)) {
        let read = self.machines.clouds.get(at);
        let launched = read.and_then(|read| read.launched.get(number));
        let id = launched.map(|machine| HostRef(machine.id.clone()));
        if let (Some(cloud), Some(id)) = (self.clouds().get(at).cloned(), id) {
            act(&cloud, &id);
        }
        self.check(Touched::nothing());
    }

    /// Reads every cloud after a step: the machines it launched since the last read (each
    /// booted the pool's boot time from now), the machines it no longer runs (each agent
    /// killed now), which machines' hosts the server lists, and the calls recorded since,
    /// into the observations. Nothing for a cell without an elastic pool.
    pub(super) fn read_cloud(&mut self) {
        let clouds = self.clouds().to_vec();
        for (at, cloud) in clouds.iter().enumerate() {
            if self.machines.clouds.len() <= at {
                self.machines.clouds.push(Read::default());
            }
            self.read_one(at, cloud);
        }
    }

    /// Reads cloud `at`, `cloud`, as [`Cell::read_cloud`] says.
    fn read_one(&mut self, at: usize, cloud: &Cloud) {
        let fake = cloud.fake();
        let truth = fake.truth();
        let known: BTreeSet<String> = truth.iter().map(|m| m.id.as_str().to_owned()).collect();
        for n in self.machines.clouds[at].read..fake.launched() {
            let payload = Payload::read(&fake.user_data(n));
            let held = payload.and_then(|payload| {
                let name = Some(payload.host.as_str());
                let found = truth.iter().find(|machine| machine.name.as_deref() == name);
                let host = HostId::new(&payload.host).ok();
                let both = found.zip(host);
                both.map(|(held, host)| (held.clone(), payload, host))
            });
            if let Some((held, payload, host)) = held {
                self.launch(at, cloud, held, payload, host);
            }
        }
        self.machines.clouds[at].read = fake.launched();
        let running = fake.running().into_iter().map(|machine| machine.0);
        let running: BTreeSet<String> = running.collect();
        let mut stopped = Vec::new();
        let launched = self.machines.clouds[at].launched.iter_mut();
        for (number, machine) in launched.enumerate() {
            if !machine.gone && !running.contains(&machine.id) {
                machine.gone = true;
                let how = ending(cloud, &machine.id, &known);
                stopped.push((number, how, machine.host.clone(), machine.booted));
            }
        }
        for (number, how, host, booted) in stopped {
            self.stopped(cloud.named(), number, how, host, booted);
        }
        let own = running.intersection(&known).cloned().collect();
        self.refresh(at, cloud, own);
    }

    /// Notes machine `held`, launched now on cloud `at`, `cloud`, and scripts its boot.
    fn launch(&mut self, at: usize, cloud: &Cloud, held: Held, payload: Payload, host: HostId) {
        let launched = &mut self.machines.clouds[at].launched;
        let number = launched.len();
        launched.push(Launched {
            id: held.id.as_str().to_owned(),
            payload,
            host,
            acquire: held.acquire,
            booted: false,
            gone: false,
        });
        self.record(line(cloud.named(), "launch", number));
        let boot_at = self
            .sim
            .now()
            .saturating_add(cloud.pool().cloud_knobs().boot);
        self.at(boot_at, move |cell| cell.boot(at, number));
    }

    /// Starts the agent of machine `number` of cloud `at`, unless the cloud no longer runs
    /// it or its machines do not join now ([`Cloud::joins`]).
    fn boot(&mut self, at: usize, number: usize) {
        let cloud = self.clouds().get(at);
        let joins = cloud.is_some_and(Cloud::joins);
        let name = cloud.and_then(Cloud::named).map(str::to_owned);
        let name = name.as_deref();
        let machine = &mut self.machines.clouds[at].launched[number];
        if machine.gone {
            return;
        }
        if !joins {
            self.record(format!("{} silent", line(name, "boot", number)));
            return;
        }
        machine.booted = true;
        let host = machine.host.clone();
        let token = machine.payload.token.clone();
        let mut capabilities = self.spec.shape().capabilities.clone();
        // Those of the agent the boot script starts, which runs containers
        // (`--container-bin`) with the GPU toolkit (`--gpu-toolkit-hook`).
        capabilities.insert("runtime".to_owned(), "container".to_owned());
        capabilities.insert("gpu-toolkit".to_owned(), "true".to_owned());
        capabilities.insert(PER_JOB_CAPABILITY.to_owned(), "true".to_owned());
        let pool = self.spec.pool().to_owned();
        let instance = InstanceIdentity {
            provider: pool.clone(),
            instance_id: host.as_str().to_owned(),
        };
        let spec = AgentSpec {
            host: host.clone(),
            pool,
            join_token: token,
            capacity: self.capacity.clone(),
            capabilities,
            labels: BTreeMap::new(),
            hello_interval: HELLO_INTERVAL,
            worker_lag: std::time::Duration::ZERO,
            kind: AgentKind::Fake(None),
            instance: Some(instance),
        };
        let mut agent = AgentNode::new(spec, self.ledger.clone());
        agent.report_to(self.applied.clone());
        let node = node_name(name, number);
        self.machines.nodes.insert(host.clone(), node.clone());
        self.sim.add_node(node, CellNode::Agent(Box::new(agent)));
        self.record(line(name, "boot", number));
        self.check(Touched::nothing().agent(host));
    }

    /// Kills the agent of machine `number` of `host`, of the cloud named `cloud` (if it is
    /// named), which the cloud stopped running as `how` says: its host is gone, not a fault.
    /// A machine that had not `booted` has no agent to kill.
    fn stopped(
        &mut self,
        cloud: Option<&str>,
        number: usize,
        how: &str,
        host: HostId,
        booted: bool,
    ) {
        self.record(line(cloud, how, number));
        if booted {
            let name = node_name(cloud, number);
            let nodes = self.sim.nodes_mut().filter(|(id, _)| **id == name);
            let agents = nodes.filter_map(|(_, node)| node.as_agent_mut());
            agents.for_each(|agent| agent.kill());
            self.observations.agents.remove(&host);
            let at = self.sim.now();
            self.observations.kills.push(Killed { host, at });
        }
    }

    /// Writes what the invariants read of cloud `at`, `cloud`, into the observations: the
    /// scheduler's machines it runs (`running`), each machine and whether the server has
    /// listed its host, and the calls recorded since the last read and what the wrapper saw
    /// at each acquire among them.
    fn refresh(&mut self, at: usize, cloud: &Cloud, running: BTreeSet<String>) {
        let read = &mut self.machines.clouds[at];
        let calls = cloud.calls_after(read.calls);
        read.calls += calls.len();
        let seen = cloud.seen_after(read.seen);
        read.seen += seen.len();
        let crashes = calls.iter().filter(|call| call.event == CloudEvent::Crash);
        crashes.for_each(|_| self.record("cloud crash server".to_owned()));
        if self.observations.clouds.len() <= at {
            self.observations.clouds.push(facts_of(cloud));
        }
        let facts = &mut self.observations.clouds[at];
        facts.calls.extend(calls);
        facts.seen.extend(seen);
        let listed = &self.observations.server.hosts;
        let launched = self.machines.clouds[at].launched.iter();
        for (number, machine) in launched.enumerate() {
            let entry = facts.machines.entry(machine.id.clone()).or_insert(Machine {
                number,
                host: machine.host.clone(),
                joined: false,
                acquire: Some(machine.acquire.clone()),
            });
            entry.joined |= listed.contains_key(&machine.host);
        }
        facts.running = running;
    }
}

/// What the invariants read of `cloud` before any call: its pool, its share, its place
/// among the pool's providers, its `quota_backoff` and its offers.
fn facts_of(cloud: &Cloud) -> CloudFacts {
    let pool = cloud.pool();
    let facts = CloudFacts::new(pool.name(), pool.max_instances());
    let facts = facts.ranked(cloud.rank(), pool.quota_backoff());
    let offers = pool.offers().iter();
    offers.fold(facts, |facts, offer| {
        facts.with_offer(&offer.id, offer.zone.as_deref())
    })
}
