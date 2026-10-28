//! What every test module shares: the scratch directory, the fake provider, the clock
//! and the helpers that open a table.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_lease::{Boot, LeaseState, LeaseTable, ManualClock, OpenError, Settings, Timestamp};
use js_provider::Provider;
use js_provider_fake::FakeProvider;
use js_testkit::dir::TestDir;
use js_types::{
    Admission, ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape,
};

pub const T0: u64 = 1_000_000;

/// A directory under the target directory that holds one test's lease file and is
/// emptied when the test ends.
pub struct Scratch {
    pub dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease"),
            name,
        );
        Self {
            dir: owner.path().to_path_buf(),
            _owner: owner,
        }
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

/// Asserts that two scratch directories made under one name are two directories: the
/// second one's set-up does not remove the first one's file and dropping the second does
/// not remove the first. It catches a path named by the test alone, which two tests of
/// one name, or two runs of one test binary at once, would share and delete under each
/// other.
#[test]
fn scratches_of_one_name_are_separate_and_each_removes_only_itself() {
    let first = Scratch::new("same-name");
    fs::write(first.file(), "one").unwrap();
    let second = Scratch::new("same-name");
    assert_ne!(first.file(), second.file());
    assert!(first.file().exists(), "a second scratch removed the first");
    fs::write(second.file(), "two").unwrap();
    let kept = first.file();
    drop(second);
    assert_eq!(fs::read(kept).unwrap(), b"one");
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
        .with_class(ResourceClass::whole_node())
}

pub fn request(max_wall_secs: u64) -> Request {
    Request {
        class: ClassId::new("whole-node").unwrap(),
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
    vec![ResourceClass::whole_node()]
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

/// A class that is not whole-node: it books the host's every dimension but has none of
/// the whole-node waiting rules.
pub fn plain_class() -> ResourceClass {
    let id = ClassId::new("plain").unwrap();
    ResourceClass::new(id, Admission::BookAll, 0)
}

/// A fake that also serves [`plain_class`].
pub fn fake_with_plain() -> FakeProvider {
    fake().with_class(plain_class())
}

/// A request of the `gpu-host` class, which is admitted `BookAll`, not whole-node.
pub fn gpu_host_request(max_wall_secs: u64) -> Request {
    Request {
        class: ClassId::new("gpu-host").unwrap(),
        ..request(max_wall_secs)
    }
}

/// A request of the `plain` class.
pub fn plain_request(max_wall_secs: u64) -> Request {
    Request {
        class: plain_class().id().clone(),
        ..request(max_wall_secs)
    }
}
