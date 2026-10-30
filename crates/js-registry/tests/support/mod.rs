//! Fixtures the registry tests share.
#![allow(dead_code)]

pub mod world;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, Hello, Phase, ProtocolRange, Welcome, WireShape};
use js_lease::{ManualClock, Timestamp};
use js_provider::{StartAck, StartSpec};
use js_registry::{
    Credential, PoolConfig, Refusal, RegisteredProvider, Registry, SequentialSessions,
};
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
    sessions: Mutex<BTreeMap<String, String>>,
}

pub fn fixture(max_machines: usize) -> Fixture {
    fixture_of(config(max_machines))
}

/// A fixture over `config`, with session secrets from a deterministic source.
pub fn fixture_of(config: PoolConfig) -> Fixture {
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let registry = Registry::with_sessions(config, clock.clone(), SequentialSessions::new());
    let provider = registry.provider();
    Fixture {
        registry,
        provider,
        clock,
        sessions: Mutex::new(BTreeMap::new()),
    }
}

impl Fixture {
    /// Says hello through the registry and remembers the session secret issued, so
    /// [`Fixture::heartbeat`] and [`Fixture::beat`] can carry it.
    pub fn hello(&self, hello: &Hello, credential: &Credential) -> Result<Welcome, Refusal> {
        let welcome = self.registry.hello(hello, credential)?;
        self.sessions
            .lock()
            .unwrap()
            .insert(hello.host_id.clone(), welcome.session.clone());
        Ok(welcome)
    }

    /// The session secret last issued to `host` through this fixture.
    pub fn session(&self, host: &str) -> String {
        self.sessions.lock().unwrap()[host].clone()
    }

    /// A heartbeat for `host` in `epoch` carrying the session last issued to it.
    pub fn heartbeat(&self, host: &str, epoch: u64) -> Heartbeat {
        Heartbeat {
            session: self.session(host),
            ..heartbeat(host, epoch)
        }
    }

    /// Moves the clock past the grace period, so every host not heard from since is lost.
    pub fn pass_grace(&self) {
        self.clock
            .advance(Duration::from_millis(DEFAULT_GRACE_MS + 1));
    }

    /// Admits `host` with `token`, expecting it to work.
    pub fn join(&self, host: &str, token: &str) -> u64 {
        self.hello(&hello(host), &cred(token)).unwrap().epoch
    }

    pub fn beat(&self, host: &str, epoch: u64) {
        self.registry
            .heartbeat(&self.heartbeat(host, epoch), &cred("t"))
            .unwrap();
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
            disk_bytes: 0,
            gpu_class: None,
            max_leases: None,
        },
        capabilities: map(&[("os", "macos")]),
        gpu_devices: Vec::new(),
        labels: map(&[("rack", "r1")]),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
    }
}

pub fn heartbeat(host: &str, epoch: u64) -> Heartbeat {
    Heartbeat {
        host_id: host.into(),
        epoch,
        session: String::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        results: Vec::new(),
        taint: None,
        exclusive_ready: None,
        exclusive_refused: None,
        load: None,
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
