//! Fixtures the registry tests share.
#![allow(dead_code)]

pub mod world;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, Hello, Phase, ProtocolRange, Welcome, WireShape};
use js_lease::{ManualClock, Timestamp};
use js_provider::{StartAck, StartSpec};
use js_registry::{
    Credential, PoolConfig, Refusal, RegisteredProvider, Registry, SequentialSessions,
};
use js_sched::{Backfill, LoadConfig, MemoryPercent};
use js_sched_placer::engine::Settings;
use js_sched_placer::place::SchedPlacer;
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

/// The architecture the hosts of [`world::World`] report, which its pool's shape
/// ([`world_shape`]) names.
pub const WORLD_ARCH: &str = "arm64";

/// The shape of the whole-node pool of [`world::World`]: [`pool_shape`] with an `arch`
/// as well, so an engine that describes a host by its architecture (js-sched's does) can
/// describe the World's hosts.
pub fn world_shape() -> Shape {
    let shape = pool_shape();
    let mut capabilities = shape.capabilities().clone();
    capabilities.insert("arch".into(), WORLD_ARCH.into());
    Shape::new(shape.resources().clone(), capabilities)
}

/// Which engine decides a whole-node pool's queue: the registry's own pass
/// (`--placement registry`, the default) or the engine of `--placement js-sched`, a
/// [`SchedPlacer`] the registry hands its wait list at every pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Registry,
    JsSched,
}

/// Defines each named test, a `fn(Placement)` of the file, once under each placement:
/// `registry::<name>` runs it under [`Placement::Registry`] and `js_sched::<name>` under
/// [`Placement::JsSched`], with the same assertions.
#[macro_export]
macro_rules! both {
    ($($name:ident),* $(,)?) => {
        mod registry {
            $(
                #[test]
                fn $name() {
                    super::$name($crate::support::Placement::Registry)
                }
            )*
        }
        mod js_sched {
            $(
                #[test]
                fn $name() {
                    super::$name($crate::support::Placement::JsSched)
                }
            )*
        }
    };
}

pub fn config(max_machines: usize) -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), pool_shape(), max_machines)
}

/// [`config`] over [`world_shape`]: a pool whose hosts js-sched's engine can describe.
pub fn world_config(max_machines: usize) -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), world_shape(), max_machines)
}

/// A pool of up to `max_machines` hosts on a clock that moves only when a test says.
pub struct Fixture {
    pub registry: Registry,
    pub provider: RegisteredProvider,
    pub clock: ManualClock,
    /// The placer of [`Placement::JsSched`]; `None` under [`Placement::Registry`].
    pub placer: Option<Arc<SchedPlacer>>,
    sessions: Mutex<BTreeMap<String, String>>,
}

pub fn fixture(max_machines: usize) -> Fixture {
    fixture_of(config(max_machines))
}

/// A fixture over `config`, with session secrets from a deterministic source.
pub fn fixture_of(config: PoolConfig) -> Fixture {
    fixture_in(config, Placement::Registry)
}

/// A fixture over `config` under `placement`. Under [`Placement::JsSched`] the registry is
/// set up as the server sets it up for `--placement js-sched` with its defaults: it books
/// each host's whole memory ([`PoolConfig::booking_whole_memory`]), and its placer, on the
/// registry's clock and set before any host joins, is built with backfill up to any wall
/// time (no `server.max_lease_wall`), the default CPU slack and the pool's
/// `memory_schedulable_percent`.
pub fn fixture_in(config: PoolConfig, placement: Placement) -> Fixture {
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let config = match placement {
        Placement::Registry => config,
        Placement::JsSched => config.booking_whole_memory(),
    };
    let percent = u32::from(config.memory_percent().get());
    let registry = Registry::with_sessions(config, clock.clone(), SequentialSessions::new());
    let placer = (placement == Placement::JsSched).then(|| {
        let settings = Settings {
            backfill: Backfill::enforced(Duration::MAX),
            load: LoadConfig::default(),
            memory: MemoryPercent::clamped(percent),
        };
        let placer = Arc::new(SchedPlacer::new(settings, clock.clone()));
        registry.set_placer(placer.clone());
        placer
    });
    let provider = registry.provider();
    Fixture {
        registry,
        provider,
        clock,
        placer,
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

    /// Admits `host` saying hello as [`world_hello`] does, expecting it to work.
    pub fn join_world(&self, host: &str) -> u64 {
        self.hello(&world_hello(host), &cred("t")).unwrap().epoch
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
            accelerator: None,
        },
        capabilities: map(&[("os", "macos")]),
        gpu_devices: Vec::new(),
        labels: map(&[("rack", "r1")]),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
        identity: None,
    }
}

/// [`hello`] reporting the architecture [`WORLD_ARCH`] as well, as a host of a pool of
/// [`world_shape`] says it.
pub fn world_hello(host: &str) -> Hello {
    let mut said = hello(host);
    said.capabilities.insert("arch".into(), WORLD_ARCH.into());
    said
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

pub fn lease(sequence: u64) -> LeaseId {
    LeaseId::new(1, sequence)
}

/// The booking a lease file records for lease `sequence` held on `on`, booking `shares`
/// (`None` for the whole host), started at 0 and bound to end at 0.
pub fn booking(on: &str, sequence: u64, shares: Option<Resources>) -> js_lease::Booking {
    booking_at(on, sequence, shares, 0, 0)
}

/// As [`booking`], started at `started_ms` and bound to end at `ends_ms`.
pub fn booking_at(
    on: &str,
    sequence: u64,
    shares: Option<Resources>,
    started_ms: u64,
    ends_ms: u64,
) -> js_lease::Booking {
    js_lease::Booking {
        lease: lease(sequence),
        host: host(on),
        state: js_lease::LeaseState::Held,
        shares,
        started_at: Timestamp::from_millis(started_ms),
        ends_at: Timestamp::from_millis(ends_ms),
    }
}

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

pub fn spec(sequence: u64) -> StartSpec {
    StartSpec::new(lease(sequence), request(), &[class()]).unwrap()
}

pub fn started_on(ack: &StartAck) -> Option<&str> {
    match ack {
        StartAck::Started { host } => Some(host.as_str()),
        _ => None,
    }
}
