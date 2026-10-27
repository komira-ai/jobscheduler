//! Fixtures the registry tests share.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, Hello, Phase, ProtocolRange, WireShape};
use js_lease::{ManualClock, Timestamp};
use js_provider::{StartAck, StartSpec};
use js_registry::{Credential, PoolConfig, RegisteredProvider, Registry};
use js_types::{
    Admission, Capabilities, ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass,
    Resources, Shape,
};

pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn class() -> ResourceClass {
    ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0)
}

/// What the pool promises and books: 4 cores, 8 GiB, no GPU, macOS.
pub fn pool_shape() -> Shape {
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    Shape::new(resources, map(&[("os", "macos")]))
}

pub fn config(max_machines: usize) -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), pool_shape(), max_machines)
}

/// A pool of up to `max_machines` hosts on a clock that moves only when a test says.
pub struct Fixture {
    pub registry: Registry,
    pub provider: RegisteredProvider,
    pub clock: ManualClock,
}

pub fn fixture(max_machines: usize) -> Fixture {
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let registry = Registry::new(config(max_machines), clock.clone());
    let provider = registry.provider();
    Fixture {
        registry,
        provider,
        clock,
    }
}

impl Fixture {
    /// Moves the clock past the grace period, so every host not heard from since is lost.
    pub fn pass_grace(&self) {
        self.clock
            .advance(Duration::from_millis(DEFAULT_GRACE_MS + 1));
    }

    /// Admits `host` with `token`, expecting it to work.
    pub fn join(&self, host: &str, token: &str) -> u64 {
        let welcome = self.registry.hello(&hello(host), &cred(token)).unwrap();
        welcome.epoch
    }

    pub fn beat(&self, host: &str, epoch: u64) {
        self.registry.heartbeat(&heartbeat(host, epoch)).unwrap();
    }
}

pub fn cred(token: &str) -> Credential {
    Credential::of_token(token)
}

/// A hello a pool of `pool_shape` accepts: more than the pool promises, in pool
/// `builders`, serving.
pub fn hello(host: &str) -> Hello {
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: host.into(),
        pool: "builders".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            gpus: 0,
            gpu_class: None,
            max_leases: None,
        },
        capabilities: map(&[("os", "macos")]),
        gpu_devices: Vec::new(),
        labels: map(&[("rack", "r1")]),
        phase: Phase::Serving,
        held: Vec::new(),
    }
}

pub fn heartbeat(host: &str, epoch: u64) -> Heartbeat {
    Heartbeat {
        host_id: host.into(),
        epoch,
        phase: Phase::Serving,
        held: Vec::new(),
        results: Vec::new(),
        taint: None,
    }
}

pub fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

pub fn lease(sequence: u64) -> LeaseId {
    LeaseId::new(1, sequence)
}

pub fn request() -> Request {
    Request {
        class: ClassId::new("mac-host").unwrap(),
        resources: Resources::new(1000, 1, 0, None).unwrap(),
        capabilities: Capabilities::from([("os".to_string(), "macos".to_string())]),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
    }
}

pub fn spec(sequence: u64) -> StartSpec {
    StartSpec::new(lease(sequence), request(), &[class()]).unwrap()
}

pub fn started_on(ack: &StartAck) -> Option<&str> {
    match ack {
        StartAck::Started { host } => Some(host.as_str()),
        _ => None,
    }
}
