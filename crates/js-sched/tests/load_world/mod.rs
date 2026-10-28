//! A fluid model of a farm for the simulations of admission by load.
//!
//! Time is logical ticks of one second. Each running action has a demand profile in
//! millicores and a memory profile, both over its own progress. Each tick the model
//! shares a node's cores among its running actions max-min fairly (an action gets at
//! most what it demands), moves each action's progress by the share it got over the
//! demand it had (an idle phase passes at full speed, a starved one slows), and counts
//! the node's true demand and true memory. Every fifth tick (the window `W`) each node's
//! agent sends a reading as a heartbeat: cumulative CPU, the working set, and the
//! leases that ran the whole window. Bounds are checked against the true demand, never
//! against the scheduler's figures.

#![allow(dead_code, reason = "each test binary uses some of the world")]

use std::collections::BTreeMap;

use js_caps::NodeCaps;
use js_sched::harness::feed;
use js_sched::{
    DaemonInstance, Event, LoadConfig, Prediction, Recovered, RecoveredLease, Request, Scheduler,
};
use js_types::sched::usage::NodeLoad;
use js_types::sched::{
    Effect, Failure, FarmTime, LeaseId, LeaseKind, OperationId, Outcome, Qos, Resources, ResultRef,
    StartLease, WaiterId, WorkKey, WorkerId,
};

pub const GIB: u64 = 1 << 30;
/// The window `W`, in ticks.
pub const W: u64 = 5;

/// A kind of action: what the scheduler is told and what it really does.
#[derive(Clone, Debug)]
pub struct Spec {
    /// A label for statistics.
    pub class: &'static str,
    pub kind: LeaseKind,
    /// The cores it books.
    pub request_cpu: u64,
    /// The memory limit it first runs under.
    pub limit: u64,
    /// The prediction fed for it; `None` is charged its request's cores and no memory.
    pub hint: Option<Prediction>,
    /// Millicores demanded, by `(ticks, millicores)` over its own progress. The run
    /// ends when the profile does.
    pub demand: Vec<(u64, u64)>,
    /// Memory in use, by `(ticks, bytes)` over its own progress; the last holds on.
    pub mem: Vec<(u64, u64)>,
    pub needs: Option<js_caps::Request>,
    /// Its demand is the whole node it runs on (a pool-sized action), whatever
    /// `demand` says; `demand` still gives its length.
    pub wide: bool,
}

impl Spec {
    /// A constant demand of `millis` for `ticks`, predicted at `hint`.
    pub fn flat(class: &'static str, millis: u64, ticks: u64, hint: Prediction) -> Self {
        Self {
            class,
            kind: LeaseKind::Action,
            request_cpu: 1_000,
            limit: 8 * GIB,
            hint: Some(hint),
            demand: vec![(ticks, millis)],
            mem: vec![(ticks, 0)],
            needs: None,
            wide: false,
        }
    }

    fn total_ticks(&self) -> u64 {
        self.demand.iter().map(|(t, _)| t).sum()
    }
}

/// The demand of `r` now on a node of `cores` millicores.
fn demand_of(spec: &Spec, pos: u64, node_millis: u64) -> u64 {
    if spec.wide && pos < spec.total_ticks() * 1000 {
        node_millis
    } else {
        at(&spec.demand, pos)
    }
}

fn at(profile: &[(u64, u64)], pos_milli_ticks: u64) -> u64 {
    let mut from = 0;
    for &(ticks, value) in profile {
        from += ticks * 1000;
        if pos_milli_ticks < from {
            return value;
        }
    }
    profile.last().map_or(0, |&(_, value)| value)
}

/// How a node's agent reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reporting {
    /// A reading every window.
    Normal,
    /// Heartbeats without a reading.
    Silent,
    /// Readings that list no covered lease.
    NoCovered,
}

pub struct Node {
    pub capacity: Resources,
    caps: NodeCaps,
    pub reporting: Reporting,
    /// Bytes of file pages the agent counts in the working set on top of what the
    /// running actions use (retained active file pages).
    pub cache: u64,
    seq: u64,
    cpu_usec: u64,
    /// Most true demand in a tick, in millicores.
    pub peak_demand: u64,
    pub peak_memory: u64,
    pub peak_running: usize,
}

struct Running {
    spec_index: usize,
    node: String,
    start: u64,
    /// Progress, in thousandths of a tick.
    pos: u64,
    cpu_milli_ticks: u64,
}

/// Per class statistics.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub submitted: u64,
    pub started: u64,
    pub finished: u64,
    pub killed: u64,
    /// Ticks each start waited, in start order.
    pub waits: Vec<u64>,
}

pub struct World {
    pub s: Scheduler,
    config: LoadConfig,
    pub tick: u64,
    pub nodes: BTreeMap<String, Node>,
    specs: Vec<Spec>,
    spec_of: BTreeMap<OperationId, usize>,
    submitted_at: BTreeMap<OperationId, u64>,
    running: BTreeMap<LeaseId, Running>,
    pub stats: BTreeMap<&'static str, Stats>,
    /// Every start, in order: tick, class, node, operation, lease.
    pub starts: Vec<(u64, &'static str, String, OperationId, LeaseId)>,
    next_waiter: u64,
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn now(tick: u64) -> FarmTime {
    FarmTime::from_millis(tick * 1000)
}

impl World {
    pub fn new(config: LoadConfig) -> Self {
        Self {
            s: Scheduler::new(1).with_load_config(config),
            config,
            tick: 0,
            nodes: BTreeMap::new(),
            specs: Vec::new(),
            spec_of: BTreeMap::new(),
            submitted_at: BTreeMap::new(),
            running: BTreeMap::new(),
            stats: BTreeMap::new(),
            starts: Vec::new(),
            next_waiter: 0,
        }
    }

    pub fn add_node(&mut self, name: &str, capacity: Resources, caps: NodeCaps) {
        self.nodes.insert(
            name.to_owned(),
            Node {
                capacity,
                caps: caps.clone(),
                reporting: Reporting::Normal,
                cache: 0,
                seq: 0,
                cpu_usec: 0,
                peak_demand: 0,
                peak_memory: 0,
                peak_running: 0,
            },
        );
        self.register(name, caps);
    }

    fn register(&mut self, name: &str, caps: NodeCaps) {
        let capacity = self.nodes[name].capacity;
        feed(
            &mut self.s,
            now(self.tick),
            Event::WorkerUp {
                worker: w(name),
                instance: DaemonInstance::new(name),
                predecessor_stopped: false,
                capacity,
                caps,
            },
        );
    }

    /// A Linux container node of `cores` cores and `gib` GiB.
    pub fn linux(&mut self, name: &str, cores: u64, gib: u64) {
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .unwrap()
            .with_drivers(["container", "native-whole-machine"]);
        self.add_node(name, Resources::new(cores * 1000, gib * GIB), caps);
    }

    /// Submits one action of `spec` now.
    pub fn submit(&mut self, spec: &Spec) -> OperationId {
        let index = self.specs.len();
        self.specs.push(spec.clone());
        let n = self.next_waiter;
        self.next_waiter += 1;
        let request = Request {
            key: WorkKey::new("sim", format!("k{n}")),
            qos: Qos::Ci,
            kind: spec.kind,
            resources: Resources::new(spec.request_cpu, spec.limit),
            needs: spec.needs.clone().unwrap_or_default(),
            hermetic: true,
            do_not_cache: false,
        };
        let op = OperationId(self.s.operations() as u64);
        feed(
            &mut self.s,
            now(self.tick),
            Event::Submit {
                waiter: WaiterId(n),
                request,
            },
        );
        if let Some(hint) = spec.hint {
            self.s.set_cost(op, hint);
        }
        self.spec_of.insert(op, index);
        self.submitted_at.insert(op, self.tick);
        self.stats.entry(spec.class).or_default().submitted += 1;
        op
    }

    pub fn submit_many(&mut self, count: u64, spec: &Spec) -> Vec<OperationId> {
        (0..count).map(|_| self.submit(spec)).collect()
    }

    /// The node's true demand now, in millicores.
    pub fn demand_on(&self, node: &str) -> u64 {
        self.running
            .values()
            .filter(|r| r.node == node)
            .map(|r| {
                demand_of(
                    &self.specs[r.spec_index],
                    r.pos,
                    self.nodes[&r.node].capacity.cpu_millis,
                )
            })
            .sum()
    }

    pub fn running_on(&self, node: &str) -> usize {
        self.running.values().filter(|r| r.node == node).count()
    }

    pub fn running_of(&self, class: &str) -> usize {
        self.running
            .values()
            .filter(|r| self.specs[r.spec_index].class == class)
            .count()
    }

    /// The scheduler restarts as a new leader of term 2 and takes up what the old one held
    /// (its operations, and for each lease its cost as the grant record kept it), as a
    /// state store would give it back; the workers register again and the nodes carry
    /// on. The estimator has changed meanwhile for the keys of the leases running: asked
    /// again, it would give `predict_after` for each (the queued operations are
    /// predicted as before). A scheduler that charged a recovered lease by asking the
    /// estimator again would charge it that.
    pub fn restart(&mut self, predict_after: Prediction) {
        let old = std::mem::replace(&mut self.s, Scheduler::new(2).with_load_config(self.config));
        let leases: BTreeMap<OperationId, LeaseId> = self
            .running
            .keys()
            .map(|l| (self.s_operation_of(*l), *l))
            .map(|(op, l)| (OperationId(op), l))
            .collect();
        let queued: Vec<OperationId> = old.queued().collect();
        let operations: Vec<Recovered> = self
            .spec_of
            .keys()
            .filter(|op| leases.contains_key(op) || queued.contains(op))
            .filter_map(|op| {
                let request = old.request(*op)?.clone();
                let lease = leases.get(op).map(|l| RecoveredLease {
                    lease: *l,
                    worker: w(&self.running[l].node),
                    cost: old.lease_cost(*l),
                });
                Some(Recovered {
                    operation: *op,
                    request,
                    waiter: WaiterId(op.0),
                    lease,
                    losses: 0,
                    fate: None,
                })
            })
            .collect();
        let next = OperationId(self.spec_of.keys().map(|o| o.0 + 1).max().unwrap_or(0));
        self.s.recover(2, next, 0, operations);
        for (op, index) in &self.spec_of {
            let again = if leases.contains_key(op) {
                Some(predict_after)
            } else {
                self.specs[*index].hint
            };
            if let Some(hint) = again {
                self.s.set_cost(*op, hint);
            }
        }
        let names: Vec<String> = self.nodes.keys().cloned().collect();
        for name in names {
            let caps = self.nodes[&name].caps.clone();
            self.register(&name, caps);
        }
    }

    /// Runs one tick: placement, starts, the sharing of cores, finishes, readings.
    pub fn step(&mut self) {
        let t = self.tick;
        let effects = feed(&mut self.s, now(t), Event::Tick);
        for effect in effects {
            if let Effect::Start(start) = effect {
                self.started(&start, t);
            }
        }
        self.share_and_advance();
        self.finish_and_kill();
        self.heartbeats();
        self.tick += 1;
    }

    pub fn run(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.step();
        }
    }

    fn started(&mut self, start: &StartLease, t: u64) {
        let op = start.operation;
        let index = self.spec_of[&op];
        let class = self.specs[index].class;
        let waited = t - self.submitted_at[&op];
        let st = self.stats.entry(class).or_default();
        st.started += 1;
        st.waits.push(waited);
        self.starts
            .push((t, class, start.worker.as_str().to_owned(), op, start.lease));
        feed(
            &mut self.s,
            now(t),
            Event::Started {
                operation: op,
                lease: start.lease,
            },
        );
        self.running.insert(
            start.lease,
            Running {
                spec_index: index,
                node: start.worker.as_str().to_owned(),
                start: t,
                pos: 0,
                cpu_milli_ticks: 0,
            },
        );
    }

    fn share_and_advance(&mut self) {
        let names: Vec<String> = self.nodes.keys().cloned().collect();
        for name in names {
            let here: Vec<LeaseId> = self
                .running
                .iter()
                .filter(|(_, r)| r.node == name)
                .map(|(l, _)| *l)
                .collect();
            let node_millis = self.nodes[&name].capacity.cpu_millis;
            let demands: Vec<u64> = here
                .iter()
                .map(|l| {
                    let r = &self.running[l];
                    demand_of(&self.specs[r.spec_index], r.pos, node_millis)
                })
                .collect();
            let memory: u64 = here
                .iter()
                .map(|l| {
                    let r = &self.running[l];
                    at(&self.specs[r.spec_index].mem, r.pos)
                })
                .sum();
            let node = self.nodes.get_mut(&name).expect("node");
            let total: u64 = demands.iter().sum();
            node.peak_demand = node.peak_demand.max(total);
            node.peak_memory = node.peak_memory.max(memory);
            node.peak_running = node.peak_running.max(here.len());
            let grants = fair(&demands, node.capacity.cpu_millis);
            node.cpu_usec += grants.iter().sum::<u64>() * 1000;
            for ((lease, demand), grant) in here.iter().zip(&demands).zip(grants) {
                let r = self.running.get_mut(lease).expect("running");
                r.cpu_milli_ticks += grant;
                // An idle phase passes at full speed; a starved one slows by what it got
                // over what it asked for.
                r.pos += if *demand == 0 {
                    1000
                } else {
                    1000 * grant / demand
                };
            }
        }
    }

    fn finish_and_kill(&mut self) {
        let t = self.tick;
        let mut ended = Vec::new();
        for (lease, r) in &self.running {
            let spec = &self.specs[r.spec_index];
            let limit_now = self
                .s
                .lease_cost(*lease)
                .map_or(spec.limit, |c| c.limit_bytes);
            if at(&spec.mem, r.pos) > limit_now && limit_now > 0 {
                ended.push((*lease, false));
            } else if r.pos >= spec.total_ticks() * 1000 {
                ended.push((*lease, true));
            }
        }
        for (lease, done) in ended {
            let r = self.running.remove(&lease).expect("running");
            let op = OperationId(self.s_operation_of(lease));
            let class = self.specs[r.spec_index].class;
            let st = self.stats.entry(class).or_default();
            let outcome = if done {
                st.finished += 1;
                Outcome::Completed {
                    action_result: ResultRef::new([1; 32], op.0),
                }
            } else {
                st.killed += 1;
                Outcome::Failed(Failure::OutOfMemory)
            };
            feed(
                &mut self.s,
                now(t),
                Event::Report {
                    operation: op,
                    lease,
                    outcome,
                },
            );
        }
    }

    fn s_operation_of(&self, lease: LeaseId) -> u64 {
        self.starts
            .iter()
            .rev()
            .find(|(_, _, _, _, l)| *l == lease)
            .map(|(_, _, _, op, _)| op.0)
            .expect("a started lease")
    }

    fn heartbeats(&mut self) {
        let t = self.tick;
        let names: Vec<String> = self.nodes.keys().cloned().collect();
        for name in names {
            let held = self.s.leases_on(&w(&name));
            let reading = t % W == W - 1;
            let node = self.nodes.get_mut(&name).expect("node");
            let load = (reading && node.reporting != Reporting::Silent).then(|| {
                node.seq += 1;
                NodeLoad {
                    seq: node.seq,
                    mono_usec: (t + 1) * 1_000_000,
                    cpu_usage_usec: node.cpu_usec,
                    working_set_bytes: 0,
                    covered: Vec::new(),
                    ..NodeLoad::default()
                }
            });
            let load = load.map(|mut load| {
                let mine: Vec<&Running> = self
                    .running
                    .iter()
                    .filter(|(l, r)| r.node == name && held.contains(l))
                    .map(|(_, r)| r)
                    .collect();
                let anon: u64 = mine
                    .iter()
                    .map(|r| at(&self.specs[r.spec_index].mem, r.pos))
                    .sum();
                load.working_set_bytes = anon + self.nodes[&name].cache;
                if self.nodes[&name].reporting != Reporting::NoCovered {
                    // Ran the whole window: started before the previous reading.
                    load.covered = self
                        .running
                        .iter()
                        .filter(|(_, r)| r.node == name && r.start + W <= t)
                        .map(|(l, _)| *l)
                        .collect();
                }
                load
            });
            feed(
                &mut self.s,
                now(t),
                Event::Heartbeat {
                    worker: w(&name),
                    running: held,
                    load,
                },
            );
        }
    }
}

/// Max-min fair shares of `capacity` among `demands`.
pub fn fair(demands: &[u64], capacity: u64) -> Vec<u64> {
    let mut order: Vec<usize> = (0..demands.len()).collect();
    order.sort_by_key(|&i| demands[i]);
    let mut grants = vec![0; demands.len()];
    let mut left = capacity;
    for (done, &i) in order.iter().enumerate() {
        let share = left / (demands.len() - done) as u64;
        grants[i] = demands[i].min(share);
        left -= grants[i];
    }
    grants
}
