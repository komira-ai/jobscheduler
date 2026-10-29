//! The runner over a fake account, a manual clock, a scripted sleeper and a scripted
//! ssh.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use js_elastic::HostStatus;
use js_provider_lambdalabs::first_launch::{Remote, Seams, Sleeper, run, sync_dir};
use js_provider_lambdalabs::{
    ApiKey, Architecture, FakeLambda, InstanceId, InstanceTypeOffer, LambdaApi, ManualClock,
    Region, SetupError,
};

pub const KEY_BYTES: &str = "secret-key-bytes-4d2f";
pub const BASE: &str = "http://127.0.0.1:1";
pub const FIRST: &str = "fake0001";

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub fn offer(
    name: &str,
    price: u64,
    gpus: u32,
    arch: Architecture,
    regions: &[&str],
) -> InstanceTypeOffer {
    InstanceTypeOffer {
        name: name.to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: price,
        vcpus: 8,
        memory_gib: 64,
        storage_gib: 100,
        gpus,
        architecture: arch,
        regions: regions
            .iter()
            .map(|name| Region {
                name: (*name).to_owned(),
                description: String::new(),
            })
            .collect(),
    }
}

/// The account most tests use: the cheapest type with capacity is `gpu_1x_cheap` in
/// `region-a`.
pub fn offers() -> Vec<InstanceTypeOffer> {
    vec![
        offer(
            "gpu_1x_cheap",
            75,
            1,
            Architecture::X86_64,
            &["region-b", "region-a"],
        ),
        offer("gpu_8x_big", 1200, 8, Architecture::X86_64, &["region-a"]),
        offer("gpu_1x_arm", 10, 1, Architecture::Arm64, &["region-a"]),
        offer("gpu_1x_nocap", 5, 1, Architecture::X86_64, &[]),
        offer("cpu_4x", 1, 0, Architecture::X86_64, &["region-a"]),
        offer("gpu_1x_cheap", 75, 1, Architecture::X86_64, &["region-a"]),
        offer("Bad-Name", 1, 1, Architecture::X86_64, &["region-a"]),
    ]
}

pub fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

/// Makes the first instance active, with a public address.
pub fn activate(fake: &FakeLambda) {
    fake.update_instance(&id(FIRST), &|i| {
        i.status = HostStatus::Active;
        i.ip = Some("203.0.113.7".to_owned());
    });
}

/// What the ssh was asked: host, user, identity file, command, time limit.
pub type Asked = (String, String, PathBuf, String, Duration);

/// A scripted ssh: what it answers, and what it was asked.
pub struct TestRemote {
    pub answer: RefCell<Result<String, String>>,
    pub asked: RefCell<Vec<Asked>>,
    pub on_run: Box<dyn Fn()>,
}

impl TestRemote {
    pub fn answering(answer: Result<String, String>) -> Self {
        Self {
            answer: RefCell::new(answer),
            asked: RefCell::new(Vec::new()),
            on_run: Box::new(|| {}),
        }
    }
}

impl Remote for TestRemote {
    fn run(
        &self,
        host: &str,
        user: &str,
        identity: &Path,
        command: &str,
        limit: Duration,
        _stop: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        self.asked.borrow_mut().push((
            host.to_owned(),
            user.to_owned(),
            identity.to_owned(),
            command.to_owned(),
            limit,
        ));
        (self.on_run)();
        self.answer.borrow().clone()
    }
}

struct TestSleeper<'a> {
    clock: &'a ManualClock,
    count: Cell<u32>,
    log: &'a RefCell<Vec<Duration>>,
    hook: &'a dyn Fn(u32),
}

impl Sleeper for TestSleeper<'_> {
    fn sleep(&self, how_long: Duration) {
        self.count.set(self.count.get() + 1);
        self.log.borrow_mut().push(how_long);
        self.clock.advance(how_long);
        (self.hook)(self.count.get());
    }
}

pub struct Rig {
    pub fake: Arc<FakeLambda>,
    pub clock: Arc<ManualClock>,
    pub dir: PathBuf,
    pub key: PathBuf,
    pub state: PathBuf,
    pub sleeps: RefCell<Vec<Duration>>,
    pub connected: RefCell<Vec<String>>,
    pub bundles: RefCell<Vec<Option<PathBuf>>>,
    pub connect_error: RefCell<Option<SetupError>>,
    /// A stop request, as the example's signal handler would set it.
    pub stop: Cell<bool>,
    /// How many times the runner has read the stop request.
    pub stop_reads: Cell<u32>,
    /// The read (1-based) from which the request counts as made; 0 for never. Lets a
    /// test place the request between two of the runner's own reads.
    pub stop_from_read: Cell<u32>,
    /// The directories the runner asked to have synced, in order.
    pub synced: RefCell<Vec<PathBuf>>,
    /// The sync (1-based) that fails; 0 for none.
    pub sync_fails_at: Cell<usize>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lambdalabs-first-launch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        fs::write(&key, format!("{KEY_BYTES}\n")).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let fake = Arc::new(FakeLambda::new());
        fake.set_offers(offers());
        Self {
            fake,
            clock: Arc::new(ManualClock::new()),
            state: dir.join("state"),
            key,
            dir,
            sleeps: RefCell::new(Vec::new()),
            connected: RefCell::new(Vec::new()),
            bundles: RefCell::new(Vec::new()),
            connect_error: RefCell::new(None),
            stop: Cell::new(false),
            stop_reads: Cell::new(0),
            stop_from_read: Cell::new(0),
            synced: RefCell::new(Vec::new()),
            sync_fails_at: Cell::new(0),
        }
    }

    /// The flags every run has, then `extra`.
    pub fn args(&self, extra: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = [
            "--base-url",
            BASE,
            "--key-file",
            self.key.to_str().unwrap(),
            "--key-name",
            "my-key",
            "--state-file",
            self.state.to_str().unwrap(),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    }

    /// Runs the program with `args`, `hook` called after each sleep with its number.
    pub fn exec_with(
        &self,
        args: &[String],
        hook: &dyn Fn(u32),
        remote: &TestRemote,
    ) -> (i32, String) {
        let sleeper = TestSleeper {
            clock: &self.clock,
            count: Cell::new(0),
            log: &self.sleeps,
            hook,
        };
        let connect = |base: &str, _key: &ApiKey, ca: Option<&Path>| {
            self.connected.borrow_mut().push(base.to_owned());
            self.bundles.borrow_mut().push(ca.map(Path::to_path_buf));
            match self.connect_error.borrow().clone() {
                Some(why) => Err(why),
                None => Ok(self.fake.clone() as Arc<dyn LambdaApi>),
            }
        };
        let wall = || 1_000;
        let stop = || {
            self.stop_reads.set(self.stop_reads.get() + 1);
            self.stop.get()
                || (self.stop_from_read.get() != 0
                    && self.stop_reads.get() >= self.stop_from_read.get())
        };
        let sync = |dir: &Path| {
            self.synced.borrow_mut().push(dir.to_path_buf());
            match self.sync_fails_at.get() == self.synced.borrow().len() {
                true => Err(std::io::Error::other("sync refused")),
                false => sync_dir(dir),
            }
        };
        let seams = Seams {
            clock: self.clock.clone(),
            sleeper: &sleeper,
            remote,
            wall_unix: &wall,
            sync_dir: &sync,
            connect: &connect,
            stop: &stop,
        };
        let mut out = Vec::new();
        let status = run(args, &seams, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains(KEY_BYTES),
            "the key is in the output:\n{text}"
        );
        (status, text)
    }

    /// Runs with no hook and an ssh that must not be asked.
    pub fn exec(&self, args: &[String]) -> (i32, String) {
        let remote = TestRemote::answering(Err("not asked".to_owned()));
        let out = self.exec_with(args, &|_| {}, &remote);
        assert!(remote.asked.borrow().is_empty());
        out
    }

    /// A run whose instance becomes active after `after` sleeps.
    pub fn exec_active(&self, args: &[String], after: u32, remote: &TestRemote) -> (i32, String) {
        let fake = self.fake.clone();
        self.exec_with(
            args,
            &move |n| {
                if n == after {
                    activate(&fake);
                }
            },
            remote,
        )
    }

    /// The plan hash a dry run prints for the flags `extra`.
    pub fn hash(&self, extra: &[&str]) -> String {
        let (status, out) = self.exec(&self.args(extra));
        assert_eq!(status, 0, "{out}");
        hash_of(&out)
    }

    /// A launch with the right plan hash.
    pub fn go_args(&self, extra: &[&str]) -> Vec<String> {
        let hash = self.hash(extra);
        let mut all = self.args(extra);
        all.extend(["--go".to_owned(), "--plan-hash".to_owned(), hash]);
        all
    }

    pub fn state_text(&self) -> String {
        fs::read_to_string(&self.state).unwrap()
    }

    pub fn status_of(&self, id_text: &str) -> HostStatus {
        self.fake
            .instances()
            .into_iter()
            .find(|i| i.id == id(id_text))
            .unwrap()
            .status
    }
}

pub fn hash_of(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("plan hash: "))
        .unwrap()
        .to_owned()
}

impl Rig {
    /// Seconds the manual clock has moved.
    pub fn clock_secs(&self) -> u64 {
        use js_provider_lambdalabs::Clock;
        self.clock.now().as_secs()
    }
}

impl Rig {
    /// The base URL and key file only, then `extra`.
    pub fn args_without_plan(&self, extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "--base-url".to_owned(),
            BASE.to_owned(),
            "--key-file".to_owned(),
            self.key.to_str().unwrap().to_owned(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    }
}
