//! What every test module shares: the scratch directory, the fake provider, the clock
//! and the helpers that open a table.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, OpenError, Settings, Timestamp};
use js_provider::Provider;
use js_provider_fake::FakeProvider;
use js_types::{ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape};

pub const T0: u64 = 1_000_000;

/// A directory under the target directory that holds one test's lease file and is
/// emptied when the test ends.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-lease")
            .join(name);
        fs::create_dir_all(&dir).unwrap();
        let scratch = Self { dir };
        scratch.clear();
        scratch
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("leases.toml")
    }

    pub fn clear(&self) {
        let _ = fs::remove_dir(self.dir.join("leases.toml.tmp"));
        for name in ["leases.toml", "leases.toml.tmp", "leases.toml.lock"] {
            let _ = fs::remove_file(self.dir.join(name));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.clear();
        let _ = fs::remove_dir(&self.dir);
    }
}

pub type Table = LeaseTable<FakeProvider, ManualClock>;

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

pub fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

pub fn settings() -> Settings {
    Settings::new(secs(60), secs(120)).unwrap()
}

pub fn clock() -> ManualClock {
    ManualClock::new(at(T0))
}

fn shape() -> Shape {
    let resources = Resources::new(8000, 8, 1, Some("g".into())).unwrap();
    Shape::new(resources, BTreeMap::new())
}

pub fn fake() -> FakeProvider {
    let hosts = vec![(host("h1"), shape()), (host("h2"), shape())];
    FakeProvider::new(ProviderId::new("fake").unwrap(), hosts)
}

pub fn request(max_wall_secs: u64) -> Request {
    Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: Resources::new(1000, 1, 1, Some("g".into())).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: secs(max_wall_secs),
        labels: BTreeMap::new(),
        validation_run: None,
    }
}

pub fn open(provider: FakeProvider, clock: &ManualClock, scratch: &Scratch) -> Table {
    LeaseTable::open(
        provider,
        clock.clone(),
        settings(),
        scratch.file(),
        Boot::Initialise,
    )
    .unwrap()
}

/// Opens a table over any provider, on a clock of its own.
pub fn open_with<P: Provider>(provider: P, scratch: &Scratch) -> LeaseTable<P, ManualClock> {
    let boot = Boot::Initialise;
    LeaseTable::open(provider, clock(), settings(), scratch.file(), boot).unwrap()
}

pub fn fresh(name: &str) -> (Table, ManualClock, Scratch) {
    let scratch = Scratch::new(name);
    let clock = clock();
    (open(fake(), &clock, &scratch), clock, scratch)
}

pub fn states<P: Provider>(table: &LeaseTable<P, ManualClock>) -> Vec<(LeaseId, LeaseState)> {
    table.list().into_iter().map(|l| (l.id, l.state)).collect()
}

pub fn provider_classes() -> Vec<ResourceClass> {
    vec![ResourceClass::gpu_host()]
}

pub fn open_err(path: &Path) -> OpenError {
    match LeaseTable::open(fake(), clock(), settings(), path, Boot::Initialise) {
        Err(err) => err,
        Ok(_) => panic!("opened over {}", path.display()),
    }
}

/// Removes the lease file, its lock and the directory, so the next write fails.
pub fn break_dir(scratch: &Scratch) {
    scratch.clear();
    fs::remove_dir(&scratch.dir).unwrap();
}
