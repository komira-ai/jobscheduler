//! The rig of the placement engine's tests: the agent side of a server assembled from the
//! flags as the server assembles it, on a manual clock, with the hosts it registers and
//! the provider and lease table it leases through. No listener is bound.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Heartbeat, Hello, LoadDrops, NodeLoad as Wire, Phase};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Pool, StartSpec};
use js_registry::{Credential, RegisteredProvider};
use js_testkit::dir::TestDir;
use js_types::{LeaseId, Request, Resources};
use serde_json::json;

use super::place::{Fallbacks, SchedPlacer};
use super::wiring_tests::{JOIN, config_of, host_of, scratch, sharing, sharing_spec};
use crate::adapt::SharedClock;
use crate::agents::{Agents, assemble_placed};

/// The time the clock starts at.
pub(super) const T0: u64 = 1_000_000;

/// One GiB.
pub(super) const GIB: u64 = 1 << 30;

/// An agent pool of up to `machines` hosts of `cpu_millis` and 16 GiB, with an `arch`,
/// and `extra` keys first.
pub(super) fn pool(machines: usize, cpu_millis: u64, extra: &str) -> String {
    format!(
        r#"{extra}
name = "burst"
class = "burst-host"
max_machines = {machines}

[shape]
cpu_millis = {cpu_millis}
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#
    )
}

/// A hello of `host` claiming `cpu_millis` and `memory_bytes`.
pub(super) fn hello_of(host: &str, cpu_millis: u64, memory_bytes: u64) -> Hello {
    let body = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": cpu_millis, "memory_bytes": memory_bytes, "gpus": 0 },
        "capabilities": { "arch": "x86_64" },
        "labels": {},
        "phase": "serving",
    });
    serde_json::from_value(body).unwrap()
}

/// The agent side of a server, the provider of its registry serving the class `share`,
/// its clock and each registered host's session.
pub(super) struct Rig {
    pub(super) agents: Arc<Agents>,
    pub(super) placer: Option<Arc<SchedPlacer>>,
    /// What the server builds its lease table from: the capacity source, its settings,
    /// its file and how it boots.
    source: Box<dyn crate::CapacitySource>,
    table_of: (js_lease::Settings, std::path::PathBuf, js_lease::Boot),
    pub(super) provider: RegisteredProvider,
    pub(super) clock: ManualClock,
    sessions: Vec<(String, u64, String)>,
    _dir: TestDir,
}

/// [`Rig`] over the agent pool `pool` with `extra` flags, the hosts `hosts` registered at
/// [`T0`] with hellos claiming 16 cores and 32 GiB.
pub(super) fn rig(name: &str, pool: &str, hosts: &[&str], extra: &[&str]) -> Rig {
    let dir = scratch(name);
    let config = config_of(dir.path(), pool, extra);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let mut rig = rig_of(config, dir, clock);
    for host in hosts {
        rig.hello(&hello_of(host, 16_000, 32 * GIB));
    }
    rig
}

/// [`Rig`] over `config`, in `dir`, on `clock`, with no host registered.
pub(super) fn rig_of(config: crate::config::Config, dir: TestDir, clock: ManualClock) -> Rig {
    let shared = SharedClock::new(clock.clone());
    let table_of = (config.settings, config.state_file.clone(), config.boot);
    let ((source, agents), placer) =
        assemble_placed(config.hosts, config.usage, &shared, &config.state_file).unwrap();
    let agents = agents.unwrap();
    let provider = agents.registry.provider().with_classes(vec![sharing()]);
    Rig {
        agents,
        placer,
        source,
        table_of,
        provider,
        clock,
        sessions: Vec::new(),
        _dir: dir,
    }
}

impl Rig {
    /// The placer the registry was given.
    pub(super) fn placer(&self) -> &SchedPlacer {
        self.placer.as_deref().unwrap()
    }

    /// Says `hello`, which the registry admits; its session replaces any the host had.
    pub(super) fn hello(&mut self, hello: &Hello) {
        let credential = Credential::of_token(JOIN);
        let welcome = self.agents.registry.hello(hello, &credential).unwrap();
        let entry = (hello.host_id.clone(), welcome.epoch, welcome.session);
        self.sessions.retain(|(host, ..)| *host != entry.0);
        self.sessions.push(entry);
    }

    /// The lease table the server runs over this agent side, on the same clock, opened as
    /// the server opens it: on a resume the provider is given the bookings the lease file
    /// names first, which the registry puts back before any host says hello.
    pub(super) fn table(&self) -> js_lease::LeaseTable<crate::adapt::BoxedPool, SharedClock> {
        let (settings, file, boot) = self.table_of.clone();
        let bookings = match boot {
            js_lease::Boot::Resume => js_lease::read_bookings(&file).unwrap(),
            js_lease::Boot::Initialise => Vec::new(),
        };
        let seated = self.source.provider(&bookings).unwrap();
        let provider = crate::adapt::BoxedPool(seated.provider);
        let clock = SharedClock::new(self.clock.clone());
        js_lease::LeaseTable::open(provider, clock, settings, &file, boot).unwrap()
    }

    /// This server stopped, and another started over its files with `config` (which boots
    /// as `resume`), on the same clock: a new registry and a new engine, nothing carried
    /// over but the files. No host has said hello to it yet.
    pub(super) fn restarted(self, config: crate::config::Config) -> Self {
        let Self {
            agents,
            placer,
            source,
            provider,
            clock,
            _dir,
            ..
        } = self;
        // The old server lets go of its files (the usage file's lock) before the new one
        // opens them.
        drop((agents, placer, source, provider));
        rig_of(config, _dir, clock)
    }

    /// The directory of the server's files.
    pub(super) fn dir(&self) -> &std::path::Path {
        self._dir.path()
    }

    /// A heartbeat of `host` with `load`, which the registry accepts.
    pub(super) fn beat(&self, host: &str, load: Option<Wire>) {
        let beat = self.heartbeat(host, load);
        let credential = Credential::of_token(JOIN);
        self.agents.registry.heartbeat(&beat, &credential).unwrap();
    }

    /// The heartbeat of `host`'s present session, with `load`.
    pub(super) fn heartbeat(&self, host: &str, load: Option<Wire>) -> Heartbeat {
        let (_, epoch, session) = self.sessions.iter().find(|(h, ..)| h == host).unwrap();
        Heartbeat {
            host_id: host.to_owned(),
            epoch: *epoch,
            session: session.clone(),
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: None,
            exclusive_refused: None,
            load,
            disk_gib: None,
            jobs: Vec::new(),
            retired_jobs: Vec::new(),
        }
    }

    /// Moves the clock on by `ms`.
    pub(super) fn advance(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    /// Starts `spec`; the host it is booked on.
    pub(super) fn start(&mut self, spec: StartSpec) -> Option<String> {
        host_of(&self.provider.start(spec)).map(str::to_owned)
    }

    /// The counters of the placer, as one value: fallbacks, resyncs and engines built.
    pub(super) fn counted(&self) -> (Fallbacks, u64, u64) {
        let placer = self.placer();
        (placer.fallbacks(), placer.resynced(), placer.built())
    }
}

/// No fallback.
pub(super) const NONE: Fallbacks = Fallbacks {
    unmappable: 0,
    declined: 0,
};

/// A lease of class `share` numbered `sequence`, booking `cpu_millis` and `memory_bytes`.
pub(super) fn share_of(sequence: u64, cpu_millis: u64, memory_bytes: u64) -> StartSpec {
    let spec = sharing_spec(sequence);
    let request = Request {
        resources: Resources::new(cpu_millis, memory_bytes, 0, None).unwrap(),
        ..spec.request().clone()
    };
    StartSpec::new(LeaseId::new(1, sequence), request, &[sharing()]).unwrap()
}

/// The hosts `h01` to `h<count>`.
pub(super) fn names(count: usize) -> Vec<String> {
    (1..=count).map(|n| format!("h{n:02}")).collect()
}

/// A load reading numbered `seq`, `seq` windows of 5 s into the agent's clock, with the
/// host's leases busy at `cores` thousandths of a core on average since the start, a
/// working set of `working_set` bytes, and `covered` the leases it covers.
pub(super) fn reading(seq: u64, cores: u64, working_set: u64, covered: &[&str]) -> Wire {
    Wire {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 5 * cores * 1_000,
        working_set_bytes: working_set,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: covered.iter().map(|lease| (*lease).to_owned()).collect(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: LoadDrops::default(),
    }
}
