//! Fixtures the registry tests share.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, Hello, Phase, ProtocolRange, Welcome, WireShape};
use js_registry::{Credential, Placer, PoolConfig, Refusal, Registry, SequentialSessions};
use js_sched::{Backfill, LoadConfig, MemoryPercent};
use js_sched_placer::engine::Settings;
use js_sched_placer::place::SchedPlacer;
use js_types::{
    Admission, Capabilities, ClassId, HostId, ProviderId, Request, ResourceClass, Resources, Shape,
};
use js_types::{ManualClock, Timestamp};

pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn class() -> ResourceClass {
    ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0)
}

/// The architecture the pool's shape names and every test host reports: the engine of the
/// placer (js-sched's) describes a host by its architecture, and refuses a shape without one.
pub const ARCH: &str = "arm64";

/// What the pool promises and books: 4 cores, 8 GiB, no GPU, macOS on [`ARCH`].
pub fn pool_shape() -> Shape {
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    Shape::new(resources, map(&[("os", "macos"), ("arch", ARCH)]))
}

pub fn config(max_machines: usize) -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), pool_shape(), max_machines)
}

/// The placer the server gives a registry, with its defaults: backfill up to any wall time
/// (no `server.max_lease_wall`), the default CPU slack and `percent` of each host's memory,
/// on `clock`.
pub fn sched_placer(percent: u32, clock: &ManualClock) -> Arc<SchedPlacer> {
    let settings = Settings {
        backfill: Backfill::enforced(Duration::MAX),
        load: LoadConfig::default(),
        memory: MemoryPercent::clamped(percent),
    };
    Arc::new(SchedPlacer::new(settings, clock.clone()))
}

/// A pool of up to `max_machines` hosts on a clock that moves only when a test says.
pub struct Fixture {
    pub registry: Registry,
    pub clock: ManualClock,
    /// The registry's placer ([`sched_placer`]).
    pub placer: Arc<SchedPlacer>,
    sessions: Mutex<BTreeMap<String, String>>,
}

pub fn fixture(max_machines: usize) -> Fixture {
    fixture_of(config(max_machines))
}

/// A fixture over `config`, with session secrets from a deterministic source, set up as the
/// server sets a registry up: it books each host's whole memory
/// ([`PoolConfig::booking_whole_memory`]), and its placer ([`sched_placer`], on the
/// registry's clock, given at construction) holds each host to the pool's
/// `memory_schedulable_percent`.
pub fn fixture_of(config: PoolConfig) -> Fixture {
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let config = config.booking_whole_memory();
    let percent = u32::from(config.memory_percent().get());
    let placer = sched_placer(percent, &clock);
    let sessions = SequentialSessions::new();
    let registry = Registry::with_sessions(config, clock.clone(), sessions, placer.clone());
    Fixture {
        registry,
        clock,
        placer,
        sessions: Mutex::new(BTreeMap::new()),
    }
}

/// A fixture as [`fixture_of`] builds it, but with `placer` as the registry's placer in place
/// of the production one.
pub fn with_placer(config: PoolConfig, placer: Arc<dyn Placer>) -> Fixture {
    let mut fx = fixture_of(config.clone());
    fx.registry = Registry::with_sessions(
        config.booking_whole_memory(),
        fx.clock.clone(),
        SequentialSessions::new(),
        placer,
    );
    fx
}

/// A placer that holds work on the hosts it is told to, as the engine holds a job's
/// allocation, and takes everything else as the defaults do.
#[derive(Default)]
pub struct Holding {
    hosts: Mutex<BTreeSet<HostId>>,
}

impl Holding {
    /// Holds work on `host` from now on.
    pub fn hold(&self, host: &HostId) {
        self.hosts.lock().unwrap().insert(host.clone());
    }

    /// Holds no work on `host` any more.
    pub fn release(&self, host: &HostId) {
        self.hosts.lock().unwrap().remove(host);
    }
}

impl Placer for Holding {
    fn holds_work_on(&self, host: &HostId) -> bool {
        self.hosts.lock().unwrap().contains(host)
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
/// `builders`, serving, on [`ARCH`].
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
            accelerator: None,
        },
        capabilities: map(&[("os", "macos"), ("arch", ARCH)]),
        gpu_devices: Vec::new(),
        labels: map(&[("rack", "r1")]),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
        identity: None,
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
        disk_gib: None,
        jobs: Vec::new(),
        retired_jobs: Vec::new(),
        log_grant_state: None,
    }
}

pub fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A request of the pool's class for one core and a byte, which requires macOS.
pub fn request() -> Request {
    Request {
        class: ClassId::new("mac-host").unwrap(),
        resources: Resources::new(1000, 1, 0, None).unwrap(),
        capabilities: Capabilities::from([("os".to_string(), "macos".to_string())]),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    }
}

/// Whether a job asking for `request` is placed on the registered host `name`: the
/// scheduler's picture of the host, from the shape the registry places it on
/// ([`js_registry::HostView::placement_shape`]), meets what the request needs.
pub fn placed_on(fx: &Fixture, name: &str, request: &Request) -> bool {
    let hosts = fx.registry.hosts();
    let host = hosts.iter().find(|host| host.id.as_str() == name).unwrap();
    let (_, node) = js_sched_placer::map::host(&host.placement_shape()).unwrap();
    let needs = js_sched_placer::map::needs_of(&request.capabilities, &request.resources);
    needs.is_ok_and(|needs| needs.matches(&node))
}
