//! A host the registry refuses to remove, across a restart of the scaler: the host is
//! written to the file of hosts the registry would not remove, and the first tick of the
//! next scaler tries again. The registry's answer, the provider and the disk of the records
//! are the fake world's; the file of unremoved hosts is the real one.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, Demand, Held, HostRef, Offer, ProviderError, Released, SchedulerId, Secret,
};
use js_jobs::JobStoreError;
use js_lease_server::{Call, Effects, JobTables, Scaler, SharedClock, UnremovedFile};
use js_registry::{Busy, HostState, HostView, SequentialSessions};
use js_scaler::{AcquirePlan, Health, Phase as Stage, Records};
use js_testkit::dir::TestDir;
use js_types::ExecMode;
use js_types::{ManualClock, Timestamp};

use crate::fakes::{Handle, START, live, new_world, record, records, view};

const HOST: &str = "js-real";

/// The fake world for the registry questions, the marks and the rest, and the real file for
/// the hosts the registry refused to remove.
struct Wired {
    fake: Handle,
    file: UnremovedFile,
}

impl Effects for Wired {
    fn hosts(&self) -> Vec<HostView> {
        self.fake.hosts()
    }

    fn jobs(&self) -> Result<JobTables, JobStoreError> {
        self.fake.jobs()
    }

    fn whole_host(&self) -> Demand {
        self.fake.whole_host()
    }

    fn holds_work(&self, host: &str) -> bool {
        self.fake.holds_work(host)
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
        self.fake.remove(host)
    }

    fn forget(&self, host: &str) {
        self.fake.forget(host);
    }

    fn call(&self, call: Call, host: &str) {
        self.fake.call(call, host);
    }
}

fn dir(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server"),
        name,
    )
}

/// The record of the machine of `HOST` as `phase`, gone from the provider.
fn machine(phase: Stage) -> Records {
    let mut machine = record(HOST, phase);
    machine.cloud_id = Some("i-gone".to_owned());
    machine.registered = true;
    records(vec![machine])
}

/// A scaler for the pool `gpu` over `world`, starting from `start` (the records) and
/// writing the hosts the registry refused to remove to `file`, in a world whose provider
/// no longer has the machine. `queued` is what [`UnremovedFile::load`] read, when it is
/// read.
fn scaler(
    (world, clock): (&Handle, &ManualClock),
    (file, start): (&Path, Records),
    queued: Option<BTreeSet<String>>,
) -> Scaler {
    let effects = Wired {
        fake: Handle(world.0.clone()),
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
    scaler
}

/// A world whose provider has lost the machine of `HOST`, whose registry holds the host, silent
/// past its grace, and refuses to remove it.
fn refusing_world() -> Handle {
    let world = new_world();
    {
        let mut inside = world.lock().unwrap();
        inside.gone = vec!["i-gone".to_owned()];
        let lost = HostView {
            state: HostState::Lost,
            ..view(HOST)
        };
        inside.hosts = vec![lost];
        inside.remove_ok = false;
    }
    Handle(world)
}

fn held(world: &Handle) -> Vec<String> {
    let hosts = world.hosts();
    hosts.iter().map(|each| each.id.to_string()).collect()
}

fn mode(path: &PathBuf) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The first scaler: its machine is gone and the registry refuses to remove the host; the
/// host is written to the file. Returns the file's path, the world, the clock and the
/// records the first scaler wrote, with the first scaler gone, as a stopped server leaves
/// them.
async fn first_run(dir: &TestDir) -> (PathBuf, Handle, (ManualClock, Records)) {
    let path = dir.path().join("state.scaler.unremoved");
    let clock = ManualClock::new(Timestamp::from_millis(START));
    let world = refusing_world();
    let start = machine(Stage::Registered);
    let first = scaler((&world, &clock), (&path, start), None);
    first.tick().await;
    first.tick().await;
    assert_eq!(held(&world), [HOST], "the registry refused the removal");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(
        UnremovedFile::new(path.clone()).load().unwrap(),
        BTreeSet::from([HOST.to_owned()])
    );
    let written = world.0.lock().unwrap().saved.clone().unwrap();
    (path, world, (clock, written))
}

/// Asserts that a removal the registry refused is retried after a restart: the first
/// scaler asks for the removal of the host of a gone machine and the registry refuses, and
/// the file lists the host. A second scaler, started on the machine's record (already past
/// the point where the step asks for a removal) and on the hosts the file lists, retries at
/// its first tick and is refused again (the file still lists the host); when the registry
/// accepts, the next tick removes the host and the file lists none. It catches a refusal
/// that a restart forgets, which leaves the host and its slot until the registry's own loss
/// timer runs out, and an entry that stays in the file after the removal.
#[tokio::test]
async fn a_removal_the_registry_refused_is_retried_after_a_restart() {
    let dir = dir("unremoved_restart_retry");
    let (path, world, (clock, written)) = first_run(&dir).await;

    let queued = UnremovedFile::new(path.clone()).load().unwrap();
    let second = scaler((&world, &clock), (&path, written), Some(queued));
    second.tick().await;
    assert_eq!(held(&world), [HOST]);
    assert_eq!(
        UnremovedFile::new(path.clone()).load().unwrap(),
        BTreeSet::from([HOST.to_owned()])
    );

    world.0.lock().unwrap().remove_ok = true;
    second.tick().await;
    assert!(held(&world).is_empty(), "{:?}", held(&world));
    assert_eq!(UnremovedFile::new(path).load().unwrap(), BTreeSet::new());
}

/// Asserts what the test above would show without the file: a second scaler started on
/// the same records and not told of the host never asks the registry again, and the host
/// is still there after the registry would accept the removal and ticks have passed. It
/// keeps the test above honest: that the host leaves there is the file's doing and not the
/// step's.
#[tokio::test]
async fn without_the_file_a_restart_leaves_the_host() {
    let dir = dir("unremoved_restart_control");
    let (path, world, (clock, written)) = first_run(&dir).await;

    let second = scaler((&world, &clock), (&path, written), None);
    world.0.lock().unwrap().remove_ok = true;
    for _ in 0..3 {
        second.tick().await;
    }
    assert_eq!(held(&world), [HOST]);
}
