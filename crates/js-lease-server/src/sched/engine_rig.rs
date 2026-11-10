//! The rig of the placement engine's tests: the agent side of a server assembled from the
//! flags as the server assembles it, on a manual clock, with the hosts it registers and the
//! job tables it places queued jobs on. No listener is bound.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Heartbeat, Hello, LoadDrops, NodeLoad as Wire, Phase};
use js_jobs::{JobId, JobSpec, Needs, NewJob};
use js_registry::Credential;
use js_testkit::dir::TestDir;
use js_types::Resources;
use js_types::{ManualClock, Timestamp};
use serde_json::json;

use super::place::SchedPlacer;
use super::wiring_tests::{JOIN, config_of, scratch};
use crate::agent_api::job_rig::{spec, tables};
use crate::agents::{Agents, assemble_placed};
use crate::clock::SharedClock;

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

/// The agent side of a server holding job tables, its clock and each registered host's
/// session.
pub(super) struct Rig {
    pub(super) agents: Arc<Agents>,
    pub(super) clock: ManualClock,
    placer: Arc<SchedPlacer>,
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
    let (agents, placer) = assemble_placed(
        config.agents,
        config.usage,
        &shared,
        &config.state_file,
        None,
    )
    .unwrap();
    let agents = Arc::try_unwrap(agents).ok().unwrap();
    let (_store, jobs) = tables();
    let agents = Arc::new(agents.with_jobs(Arc::new(jobs)));
    Rig {
        agents,
        clock,
        placer,
        sessions: Vec::new(),
        _dir: dir,
    }
}

impl Rig {
    /// The placer the registry was given.
    pub(super) fn placer(&self) -> &SchedPlacer {
        &self.placer
    }

    /// Says `hello`, which the registry admits; its session replaces any the host had.
    pub(super) fn hello(&mut self, hello: &Hello) {
        let credential = Credential::of_token(JOIN);
        let welcome = self.agents.registry.hello(hello, &credential).unwrap();
        let entry = (hello.host_id.clone(), welcome.epoch, welcome.session);
        self.sessions.retain(|(host, ..)| *host != entry.0);
        self.sessions.push(entry);
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
            log_grant_state: None,
        }
    }

    /// Moves the clock on by `ms`.
    pub(super) fn advance(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    /// Queues job `id` asking for `cpu_millis` and `memory_bytes`, runs a pass over the
    /// queue, and returns the host the job was placed on; `None` when no host took it, in
    /// which case the job is cancelled, so that it holds back none of the jobs after it.
    pub(super) fn place(&self, id: &str, cpu_millis: u64, memory_bytes: u64) -> Option<String> {
        let jobs = self.agents.jobs.as_ref().unwrap();
        let spec = JobSpec {
            needs: Needs::Shape(Resources::new(cpu_millis, memory_bytes, 0, None).unwrap()),
            ..spec()
        };
        let new = NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec,
            priority: 0,
            client: None,
        };
        let queued = jobs.insert_queued(new).unwrap().record.id;
        self.agents.place_jobs();
        let allocation = jobs.get_allocation(&queued).unwrap();
        if allocation.is_none() {
            jobs.request_cancel(&queued).unwrap();
        }
        allocation.map(|allocation| allocation.host.to_string())
    }
}

/// A load reading numbered `seq`, `seq` windows of 5 s into the agent's clock, with the
/// host busy at `cores` thousandths of a core on average since the start and a working set
/// of `working_set` bytes.
pub(super) fn reading(seq: u64, cores: u64, working_set: u64) -> Wire {
    Wire {
        seq,
        mono_usec: seq * 5_000_000,
        cpu_usage_usec: seq * 5 * cores * 1_000,
        working_set_bytes: working_set,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: Vec::new(),
        dropped: LoadDrops::default(),
    }
}
