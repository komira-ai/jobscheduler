//! The run loop's side of an elastic pool (see the module doc of [`crate::elastic`]): after
//! every step it reads the machines the cloud launched, boots an agent for each the pool's
//! boot time later, kills the agent of each the cloud no longer runs, and refreshes what
//! the invariants read of the cloud ([`CloudFacts`]).
//!
//! The trace names a machine by its number, `m<n>`, in launch order: `cloud launch m<n>`
//! when the cell first sees the cloud run it, `cloud boot m<n>` when its agent starts
//! (`cloud boot m<n> silent` for a pool that does not join, and nothing for a machine the
//! cloud no longer runs by then), and `cloud release m<n>`, `cloud preempt m<n>` or `cloud
//! drop m<n>` in the step the cloud stops running it, by the cloud's release, its
//! preemption, or the machine's disappearance; `cloud brake reset <bool>` for the
//! operator's reset of the pool's brake; and `cloud crash server` when an acquire crashed the
//! server ([`Cloud::crash_at_next_acquire`]). No line holds a payload, a token, a host's name or
//! an id the server or the cloud generated.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::InstanceIdentity;
use js_elastic::HostRef;
use js_sim::NodeId;
use js_types::HostId;

use super::{Cell, HELLO_INTERVAL};
use crate::agent::{AgentKind, AgentNode, AgentSpec};
use crate::elastic::{Cloud, CloudEvent, CloudFacts, Machine, Payload};
use crate::invariant::Touched;
use crate::node::CellNode;
use crate::observe::Killed;

/// What the cell knows of the machines of its elastic pool's cloud.
#[derive(Default)]
pub(super) struct Machines {
    /// How many of the cloud's machines (in its launch order) the cell has read.
    read: usize,
    /// How many of the cloud's calls the observations hold.
    calls: usize,
    /// Each machine the cloud launched for the scheduler, by number.
    launched: Vec<Launched>,
    /// The node of each launched machine's host.
    nodes: BTreeMap<HostId, NodeId>,
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

/// The node name of machine `number`: `m000`, `m001`, ...
fn node_name(number: usize) -> NodeId {
    NodeId::new(format!("m{number:03}"))
}

impl Cell {
    /// The elastic pool's cloud, for a cell of [`crate::ServerSpec::elastic_pool`].
    #[must_use]
    pub fn cloud(&self) -> Option<&Cloud> {
        self.server().and_then(|server| server.cloud())
    }

    /// The agent node of the cloud's machine `number` (in launch order, from 0), once it
    /// has booted.
    #[must_use]
    pub fn machine(&self, number: usize) -> Option<&AgentNode> {
        self.sim
            .node(&node_name(number))
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

    /// Makes the cloud drop machine `number`, outside the scheduler (the machine
    /// vanishes, and a read of it says so), and checks: its agent is killed in this step.
    pub fn drop_machine(&mut self, number: usize) {
        self.on_machine(number, |cloud, id| cloud.fake().drop_machine(id));
    }

    /// Makes the cloud preempt machine `number` (it stops, and reads preempted until it
    /// is released), and checks: its agent is killed in this step.
    pub fn preempt_machine(&mut self, number: usize) {
        self.on_machine(number, |cloud, id| cloud.fake().preempt(id));
    }

    fn on_machine(&mut self, number: usize, act: impl FnOnce(&Cloud, &HostRef)) {
        let launched = self.machines.launched.get(number);
        let id = launched.map(|machine| HostRef(machine.id.clone()));
        if let (Some(cloud), Some(id)) = (self.cloud().cloned(), id) {
            act(&cloud, &id);
        }
        self.check(Touched::nothing());
    }

    /// Reads the cloud after a step: the machines it launched since the last read (each
    /// booted the pool's boot time from now), the machines it no longer runs (each agent
    /// killed now), which machines' hosts the server lists, and the calls recorded since,
    /// into the observations. Nothing for a cell without an elastic pool.
    pub(super) fn read_cloud(&mut self) {
        let Some(cloud) = self.cloud().cloned() else {
            return;
        };
        let fake = cloud.fake();
        let truth = fake.truth();
        let known: BTreeSet<String> = truth.iter().map(|m| m.id.as_str().to_owned()).collect();
        for n in self.machines.read..fake.launched() {
            let payload = Payload::read(&fake.user_data(n));
            let held = payload.and_then(|payload| {
                let name = Some(payload.host.as_str());
                let found = truth.iter().find(|machine| machine.name.as_deref() == name);
                let host = HostId::new(&payload.host).ok();
                let both = found.zip(host);
                both.map(|(held, host)| (held.id.as_str().to_owned(), payload, host))
            });
            if let Some((id, payload, host)) = held {
                self.launch(&cloud, id, payload, host);
            }
        }
        self.machines.read = fake.launched();
        let running = fake.running().into_iter().map(|machine| machine.0);
        let running: BTreeSet<String> = running.collect();
        let mut stopped = Vec::new();
        for (number, machine) in self.machines.launched.iter_mut().enumerate() {
            if !machine.gone && !running.contains(&machine.id) {
                machine.gone = true;
                let how = ending(&cloud, &machine.id, &known);
                stopped.push((number, how, machine.host.clone(), machine.booted));
            }
        }
        for (number, how, host, booted) in stopped {
            self.stopped(number, how, host, booted);
        }
        let own = running.intersection(&known).cloned().collect();
        self.refresh(&cloud, own);
    }

    /// Notes machine `id`, launched now, and scripts its boot.
    fn launch(&mut self, cloud: &Cloud, id: String, payload: Payload, host: HostId) {
        let number = self.machines.launched.len();
        self.machines.launched.push(Launched {
            id,
            payload,
            host,
            booted: false,
            gone: false,
        });
        self.record(format!("cloud launch m{number:03}"));
        let at = self
            .sim
            .now()
            .saturating_add(cloud.pool().cloud_knobs().boot);
        self.at(at, move |cell| cell.boot(number));
    }

    /// Starts the agent of machine `number`, unless the cloud no longer runs it or its
    /// pool does not join.
    fn boot(&mut self, number: usize) {
        let joins = self.cloud().is_some_and(|cloud| cloud.pool().joined());
        let machine = &mut self.machines.launched[number];
        if machine.gone {
            return;
        }
        if !joins {
            self.record(format!("cloud boot m{number:03} silent"));
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
            kind: AgentKind::Fake(None),
            instance: Some(instance),
        };
        let mut agent = AgentNode::new(spec, self.ledger.clone());
        agent.report_to(self.applied.clone());
        let node = node_name(number);
        self.machines.nodes.insert(host.clone(), node.clone());
        self.sim.add_node(node, CellNode::Agent(Box::new(agent)));
        self.record(format!("cloud boot m{number:03}"));
        self.check(Touched::nothing().agent(host));
    }

    /// Kills the agent of machine `number` of `host`, which the cloud stopped running as
    /// `how` says: its host is gone, not a fault. A machine that had not `booted` has no
    /// agent to kill.
    fn stopped(&mut self, number: usize, how: &str, host: HostId, booted: bool) {
        self.record(format!("cloud {how} m{number:03}"));
        if booted {
            let name = node_name(number);
            let nodes = self.sim.nodes_mut().filter(|(id, _)| **id == name);
            let agents = nodes.filter_map(|(_, node)| node.as_agent_mut());
            agents.for_each(|agent| agent.kill());
            self.observations.agents.remove(&host);
            let at = self.sim.now();
            self.observations.kills.push(Killed { host, at });
        }
    }

    /// Writes what the invariants read of the cloud into the observations: the
    /// scheduler's machines it runs (`running`), each machine and whether the server has
    /// listed its host, and the calls recorded since the last read.
    fn refresh(&mut self, cloud: &Cloud, running: BTreeSet<String>) {
        let calls = cloud.calls_after(self.machines.calls);
        self.machines.calls += calls.len();
        let crashes = calls.iter().filter(|call| call.event == CloudEvent::Crash);
        crashes.for_each(|_| self.record("cloud crash server".to_owned()));
        let pool = cloud.pool();
        let facts = self
            .observations
            .cloud
            .get_or_insert_with(|| CloudFacts::new(pool.name(), pool.max_instances()));
        facts.calls.extend(calls);
        let listed = &self.observations.server.hosts;
        for (number, machine) in self.machines.launched.iter().enumerate() {
            let entry = facts.machines.entry(machine.id.clone()).or_insert(Machine {
                number,
                host: machine.host.clone(),
                joined: false,
            });
            entry.joined |= listed.contains_key(&machine.host);
        }
        facts.running = running;
    }
}
