//! A host the real registry refuses to remove, across a restart of the scaler: the
//! refusal is a lease booked on the host, the file keeps the host, and the first tick of
//! the next scaler tries again. The registry here is the real one (hellos, leases, marks);
//! the provider and the disk of the records are the fake world's.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Hello, Phase, ProtocolRange, WireShape};
use js_elastic::{
    AcquireId, Acquired, Held, HostRef, Offer, ProviderError, Released, SchedulerId, Secret,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{Call, Effects, Scaler, SharedClock, UnremovedFile};
use js_provider::{Provider, StartAck, StartSpec};
use js_registry::{Busy, Credential, HostView, PoolConfig, Registry, SequentialSessions, Waiting};
use js_scaler::{AcquirePlan, Health, Phase as Stage, Records};
use js_testkit::dir::TestDir;
use js_types::{
    Capabilities, ClassId, ExecMode, HostId, LeaseId, ProviderId, Request, ResourceClass,
    Resources, Shape,
};

use crate::fakes::{Handle, START, live, new_world, record, records};

const HOST: &str = "js-real";

/// The real registry for the registry questions and the marks, the fake world for the
/// rest, and the real file for the hosts the registry refused to remove.
struct Wired {
    fake: Handle,
    registry: Registry,
    file: UnremovedFile,
}

impl Effects for Wired {
    fn registry(&self) -> (Vec<HostView>, Vec<Waiting>) {
        self.registry.snapshot()
    }

    fn leased(&self, host: &str) -> bool {
        let hosts = self.registry.hosts();
        hosts
            .iter()
            .any(|each| each.id.as_str() == host && each.lease.is_some())
    }

    fn checks(&self) -> (Health, Vec<Offer>) {
        self.fake.checks()
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.fake.list()
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.fake.unresolved()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.fake.is_gone(id)
    }

    fn save(&self, records: &Records) -> Result<(), String> {
        self.fake.save(records)
    }

    fn save_unremoved(&self, hosts: &BTreeSet<String>) -> Result<(), String> {
        self.file.save(hosts).map_err(|err| err.to_string())
    }

    fn mint(&self, host: &str, valid_for: Duration) -> Option<Secret> {
        self.fake.mint(host, valid_for)
    }

    fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired {
        self.fake.acquire(plan, token)
    }

    fn release(&self, id: &HostRef) -> Released {
        self.fake.release(id)
    }

    fn revoke(&self, host: &str) -> bool {
        self.fake.revoke(host)
    }

    fn remove(&self, host: &str) -> Result<(), Busy> {
        let id = HostId::new(host).unwrap();
        self.registry.mark_ending(&id);
        self.registry.remove(&id)
    }

    fn forget(&self, host: &str) {
        self.fake.forget(host);
    }

    fn call(&self, call: Call, host: &str) {
        let id = HostId::new(host).unwrap();
        match call {
            Call::MarkEnding => self.registry.mark_ending(&id),
            Call::Drain => {
                let _ = self.registry.drain(&id);
            }
            Call::Undrain => {
                let _ = self.registry.undrain(&id);
            }
            Call::ReleaseHost => {
                let _ = self.registry.release_host(&id);
            }
        }
    }
}

fn dir(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server"),
        name,
    )
}

fn resources(cpu: u64, memory: u64) -> Resources {
    Resources::new(cpu, memory, 1, Some("nvidia-a10".to_owned())).unwrap()
}

/// A hello of the host `HOST` into the pool `gpu`, as large as the offer of the fakes.
fn hello() -> Hello {
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: HOST.to_owned(),
        pool: "gpu".to_owned(),
        agent_version: "0.1.0".to_owned(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            gpus: 1,
            gpu_class: Some("nvidia-a10".to_owned()),
            max_leases: None,
        },
        capabilities: BTreeMap::new(),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
    }
}

fn lease() -> LeaseId {
    LeaseId::new(1, 1)
}

/// A start of one a10 host under [`lease`].
fn spec() -> StartSpec {
    let request = Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: resources(1000, 1 << 30),
        capabilities: Capabilities::new(),
        max_wall: Duration::from_secs(3600),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(lease(), request, &[ResourceClass::gpu_host()]).unwrap()
}

/// A registry with the host `HOST` joined and booked under [`lease`], and then silent for
/// longer than the grace, so that its state is lost.
fn registry_with_a_lost_leased_host(clock: &ManualClock) -> Registry {
    let shape = Shape::new(resources(1000, 1 << 30), BTreeMap::new());
    let pool = ProviderId::new("gpu").unwrap();
    let config = PoolConfig::new(pool, ResourceClass::gpu_host(), shape, 4);
    let registry = Registry::with_sessions(config, clock.clone(), SequentialSessions::new());
    registry
        .hello(&hello(), &Credential::of_token("t"))
        .unwrap();
    let ack = registry.provider().start(spec());
    assert!(matches!(ack, StartAck::Started { .. }), "{ack:?}");
    clock.advance(Duration::from_millis(DEFAULT_GRACE_MS + 1));
    registry
}

/// The record of the machine of `HOST` as `phase`, gone from the provider.
fn machine(phase: Stage) -> Records {
    let mut machine = record(HOST, phase);
    machine.cloud_id = Some("i-gone".to_owned());
    machine.registered = true;
    records(vec![machine])
}

/// A scaler over `registry` for the pool `gpu`, starting from `start` (the records) and
/// writing the hosts the registry refused to remove to `file`, in a world whose provider
/// no longer has the machine. `queued` is what [`UnremovedFile::load`] read, when it is
/// read. Also returns the world, for the records the scaler wrote.
fn scaler(
    (registry, clock): (&Registry, &ManualClock),
    (file, start): (&Path, Records),
    queued: Option<BTreeSet<String>>,
) -> (Scaler, Handle) {
    let world = new_world();
    world.lock().unwrap().gone = vec!["i-gone".to_owned()];
    let effects = Wired {
        fake: Handle(world.clone()),
        registry: registry.clone(),
        file: UnremovedFile::new(file.to_path_buf()),
    };
    let scaler = Scaler::new(
        Box::new(effects),
        SharedClock::new(clock.clone()),
        Box::new(SequentialSessions::new()),
        (SchedulerId::new("sched").unwrap(), ExecMode::Container),
        live(),
        start,
    );
    if let Some(hosts) = queued {
        scaler.retry_removals(hosts);
    }
    (scaler, Handle(world))
}

fn held(registry: &Registry) -> Vec<String> {
    let hosts = registry.hosts();
    hosts.iter().map(|each| each.id.to_string()).collect()
}

fn mode(path: &PathBuf) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The first scaler: its machine is gone and the registry refuses to remove the host
/// (a lease is booked on it); the host is written to the file. Returns the file's path
/// the registry, the clock and the records the first scaler wrote, with the first scaler
/// gone, as a stopped server leaves them.
async fn first_run(dir: &TestDir) -> (PathBuf, Registry, (ManualClock, Records)) {
    let path = dir.path().join("state.scaler.unremoved");
    let clock = ManualClock::new(Timestamp::from_millis(START));
    let registry = registry_with_a_lost_leased_host(&clock);
    let start = machine(Stage::Registered);
    let (first, world) = scaler((&registry, &clock), (&path, start), None);
    first.tick().await;
    first.tick().await;
    assert_eq!(held(&registry), [HOST], "the registry refused the removal");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(
        UnremovedFile::new(path.clone()).load().unwrap(),
        BTreeSet::from([HOST.to_owned()])
    );
    let written = world.0.lock().unwrap().saved.clone().unwrap();
    (path, registry, (clock, written))
}

/// Asserts that a removal the real registry refused is retried after a restart: the
/// first scaler asks for the removal of the host of a gone machine and the registry
/// refuses because a lease is booked on it, and the file lists the host. A second scaler,
/// started on the machine's record (already past the point where the step asks for a
/// removal) and on the hosts the file lists, retries at its first tick and is refused
/// again while the lease is held (the file still lists the host); when the lease ends the
/// next tick removes the host from the registry and the file lists none. It catches a
/// refusal that a restart forgets, which leaves the host and its slot until the
/// registry's own loss timer runs out, and an entry that stays in the file after the
/// removal.
#[tokio::test]
async fn a_removal_the_registry_refused_is_retried_after_a_restart() {
    let dir = dir("unremoved_restart_retry");
    let (path, registry, (clock, written)) = first_run(&dir).await;

    let queued = UnremovedFile::new(path.clone()).load().unwrap();
    let (second, _) = scaler((&registry, &clock), (&path, written), Some(queued));
    second.tick().await;
    assert_eq!(held(&registry), [HOST]);
    assert_eq!(
        UnremovedFile::new(path.clone()).load().unwrap(),
        BTreeSet::from([HOST.to_owned()])
    );

    let _ = registry.provider().stop(&lease());
    second.tick().await;
    assert!(held(&registry).is_empty(), "{:?}", held(&registry));
    assert_eq!(UnremovedFile::new(path).load().unwrap(), BTreeSet::new());
}

/// Asserts what the test above would show without the file: a second scaler started on
/// the same records and not told of the host never asks the registry again, and the host
/// is still there after the lease has ended and ticks have passed. It keeps the test above
/// honest: that the host leaves there is the file's doing and not the step's.
#[tokio::test]
async fn without_the_file_a_restart_leaves_the_host() {
    let dir = dir("unremoved_restart_control");
    let (path, registry, (clock, written)) = first_run(&dir).await;

    let (second, _) = scaler((&registry, &clock), (&path, written), None);
    let _ = registry.provider().stop(&lease());
    for _ in 0..3 {
        second.tick().await;
    }
    assert_eq!(held(&registry), [HOST]);
}
