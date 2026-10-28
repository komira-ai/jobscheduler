//! A fake load model for scorer-level simulations: per-worker curves of the load without
//! the new lease and of the new lease's charge, at the 32 slots of the predicate, and the
//! predicate's own admission test over them. Time is the slot index (logical minutes).

#![allow(dead_code, reason = "each test binary uses some of the support")]

use std::collections::BTreeMap;

use js_sched::{Charge, HostFacts, LeaseFacts, PredictedLoad, SLOTS, ScoreConfig, Scorer};
use js_types::sched::{LeaseId, WorkerId};

pub const GIB: u64 = 1 << 30;

/// An 88-thread host in millicores.
pub const CORES_88: u64 = 88_000;

/// The predicate's cap: 110% of the CPU capacity.
pub fn cpu_cap(capacity: u64) -> u64 {
    capacity * 110 / 100
}

pub fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

#[derive(Clone)]
pub struct Host {
    pub facts: HostFacts,
    pub load: [Charge; SLOTS],
    pub new: [Charge; SLOTS],
}

impl Host {
    pub fn empty(cpu: u64, memory: u64) -> Self {
        Self {
            facts: HostFacts {
                cpu_capacity_millis: cpu,
                memory_capacity_bytes: memory,
                fit_minutes: 1,
                fallback: false,
                measured_above_predicted: false,
                same_key_millis: 0,
                new_key_millis: 0,
                uncovered_leases: 0,
            },
            load: [Charge::default(); SLOTS],
            new: [Charge::default(); SLOTS],
        }
    }

    pub fn cpu_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    pub fn cpu_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    pub fn mem_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    pub fn mem_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    pub fn life(mut self, minutes: u8) -> Self {
        self.facts.fit_minutes = minutes;
        self
    }
}

pub struct Fake {
    pub lease: LeaseFacts,
    pub hosts: BTreeMap<WorkerId, Host>,
}

impl Fake {
    pub fn new(cold: bool, hosts: Vec<(&str, Host)>) -> Self {
        Self {
            lease: LeaseFacts {
                lease: LeaseId::new(1, 2),
                cold,
                retry: false,
            },
            hosts: hosts.into_iter().map(|(n, h)| (worker(n), h)).collect(),
        }
    }

    /// The predicate over the fake: the lease fits on `name` when, at every slot, the load
    /// plus the lease's charge is within 110% of the CPU capacity and within the memory
    /// capacity.
    pub fn admits(&self, name: &str) -> bool {
        let w = worker(name);
        let host = &self.hosts[&w];
        (0..SLOTS).all(|slot| {
            let (l, n) = (host.load[slot], host.new[slot]);
            l.cpu_millis + n.cpu_millis <= cpu_cap(host.facts.cpu_capacity_millis)
                && l.memory_bytes + n.memory_bytes <= host.facts.memory_capacity_bytes
        })
    }

    /// The hosts that pass the predicate, in the given order.
    pub fn admitted(&self, order: &[&str]) -> Vec<WorkerId> {
        order
            .iter()
            .filter(|n| self.admits(n))
            .map(|n| worker(n))
            .collect()
    }

    /// The previous rule: the least loaded now (slot 0), the first by name on a tie.
    pub fn least_loaded_now(&self, candidates: &[WorkerId]) -> String {
        candidates
            .iter()
            .min_by_key(|w| (self.hosts[*w].load[0].cpu_millis, (*w).clone()))
            .expect("a candidate")
            .as_str()
            .to_owned()
    }
}

impl PredictedLoad for Fake {
    fn lease(&self) -> LeaseFacts {
        self.lease
    }

    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].load[slot]
    }

    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].new[slot]
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        self.hosts[worker].facts
    }
}

/// The same model with every slot read as slot 0: scoring the lease at `tau = 0` only.
pub struct AtSlotZero<'a>(pub &'a Fake);

impl PredictedLoad for AtSlotZero<'_> {
    fn lease(&self) -> LeaseFacts {
        self.0.lease()
    }

    fn load(&self, worker: &WorkerId, _slot: usize) -> Charge {
        self.0.load(worker, 0)
    }

    fn charge(&self, worker: &WorkerId, _slot: usize) -> Charge {
        self.0.charge(worker, 0)
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        self.0.host(worker)
    }
}

/// The worker `scorer` chooses among `candidates`.
pub fn pick_with<M: PredictedLoad>(
    scorer: &mut Scorer,
    model: &M,
    candidates: &[WorkerId],
) -> String {
    scorer
        .choose(model, candidates)
        .expect("a candidate")
        .as_str()
        .to_owned()
}

/// The worker the default score chooses.
pub fn pick<M: PredictedLoad>(model: &M, candidates: &[WorkerId]) -> String {
    pick_with(&mut Scorer::default(), model, candidates)
}

/// A score whose slack tolerance is the whole capacity range, so key 1 is the constant 0.
pub fn key_1_constant() -> Scorer {
    Scorer::new(ScoreConfig::new(1, 1_000_000_000, 250_000).expect("balance in range"))
}
