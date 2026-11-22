//! Native jobs: the runtime that starts a program as a user of the root helper, under a
//! Seatbelt profile, and the lifecycle that drives it. The helper is a fake that speaks the
//! helper's real protocol on a socket (`fake_helper`), so everything but what the verbs do
//! to a machine runs as in the agent. On a Mac, one test module applies the runtime's
//! start-mode command line for real, as the user running the tests (`mac`), one runs the
//! start mode itself (`job_start`) and one reads back that what the helper starts answers
//! to the privacy controls for itself (`disclaim`).

mod adopt;
pub(crate) mod agent;
mod bound;
mod bundle;
mod checks;
#[cfg(target_os = "macos")]
mod disclaim;
mod env;
pub(crate) mod fake_helper;
mod files;
mod grant;
mod inputs;
mod job_home;
#[cfg(target_os = "macos")]
mod job_start;
mod journal;
mod lifecycle;
mod live_logs;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod mac_agent;
#[cfg(target_os = "macos")]
mod mac_pf;
mod mounts;
mod network;
mod outputs;
mod pf_gate;
mod runtime;
mod settings;
mod silence;
mod worker;
mod xcode;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

pub use crate::support::Scratch;
use fake_helper::FakeHelper;

/// The term of the runtimes made here: their leases are `7.0`, `7.1` and so on.
pub const TERM: u64 = 7;

/// The most polls a test makes for a run to end. The helper's answer comes from a thread,
/// so a poll right after it was sent may still say the run goes on; this bounds the loop
/// instead of a clock.
pub const POLLS: u32 = 50_000_000;

/// A native runtime over a fake helper, with the directory they work in.
pub struct Rig {
    pub fake: FakeHelper,
    pub rt: NativeRuntime,
    pub dir: Scratch,
}

/// The configuration of the runtimes made here: a loopback cap, the tool roots `/usr/bin`
/// and `/bin`, and one path no run may read.
pub fn config(fake: &FakeHelper) -> NativeConfig {
    NativeConfig {
        homes: fake.homes().to_path_buf(),
        network: Ceiling::Loopback,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![AbsPath::parse("/srv/js-state").unwrap()],
        mount_roots: Vec::new(),
        read_model: ReadModel::Open,
        journal: fake
            .homes()
            .parent()
            .expect("the homes are below the test's directory")
            .join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
        start: PathBuf::from(env!("CARGO_BIN_EXE_js-agent")),
    }
}

impl Rig {
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// A rig whose configuration `adjust` has changed.
    pub fn with(name: &str, adjust: impl FnOnce(&mut NativeConfig)) -> Self {
        Self::in_dir(name, |config, _| adjust(config))
    }

    /// A rig whose configuration `adjust` has changed, knowing the directory the rig works
    /// in (the runs' scratch directories are made below `scratch` there).
    pub fn in_dir(name: &str, adjust: impl FnOnce(&mut NativeConfig, &Scratch)) -> Self {
        let dir = Scratch::new(&format!("native-{name}"));
        let fake = FakeHelper::start(&dir);
        let mut config = config(&fake);
        adjust(&mut config, &dir);
        let rt = NativeRuntime::open(
            js_agent::native::Backend::Helper(fake.client()),
            config,
            TERM,
        )
        .unwrap();
        Self { fake, rt, dir }
    }

    /// The same rig, its runtime giving a run's files to the run's user through
    /// `hand_over`.
    pub fn handing_over(self, hand_over: js_agent::native::HandOver) -> Self {
        let Self { fake, rt, dir } = self;
        let rt = rt.with_hand_over(hand_over);
        Self { fake, rt, dir }
    }

    /// The same rig, its runtime uploading the live logs of a run with a sink with `logs`.
    pub fn living(self, logs: js_agent::logs::LiveLogs) -> Self {
        let Self { fake, rt, dir } = self;
        let rt = rt.with_live_logs(logs);
        Self { fake, rt, dir }
    }

    /// The same rig, its runtime holding `xcodes`: a start may name the build of one.
    pub fn holding(self, xcodes: Vec<js_agent::xcode::Xcode>) -> Self {
        let Self { fake, rt, dir } = self;
        let rt = rt.with_xcodes(xcodes);
        Self { fake, rt, dir }
    }

    /// A run of `native`, with its scratch directory made as the lifecycle makes it.
    pub fn spec(&self, run_id: &str, native: NativeSpec) -> RunSpec {
        let scratch = self.dir.path("scratch").join(run_id);
        fs::create_dir_all(&scratch).unwrap();
        RunSpec {
            run_id: run_id.to_owned(),
            body: Body::Native(native),
            env: Vec::new(),
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            scratch,
            files: None,
            slot: 0,
            isolation: Isolation::Contained,
            fuse: false,
            labels: BTreeMap::new(),
            bundle: None,
            network: Default::default(),
            ports: Vec::new(),
            host_ports: Vec::new(),
            log_sink: None,
            xcode: None,
        }
    }

    /// Starts `program args` and returns its handle.
    pub fn start(&mut self, run_id: &str, program: &str, args: &[&str]) -> RunHandle {
        let spec = self.spec(run_id, native(program, args));
        self.rt.create_and_start(&spec).unwrap()
    }

    /// Polls until the run is no longer running.
    pub fn settle(&mut self, id: &RunHandle) -> Status {
        for _ in 0..POLLS {
            let status = self.rt.poll(id).unwrap();
            if status != Status::Running {
                return status;
            }
            std::thread::yield_now();
        }
        panic!("the run did not end: the fake helper never reported its end");
    }
}

/// A native body: `program` and `args`, no environment, no network, no inputs or outputs.
pub fn native(program: &str, args: &[&str]) -> NativeSpec {
    NativeSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    }
}

/// The command line the runtime sent the helper, taken apart: the agent (`config().start`),
/// its `__job-start` word, `--profile` and the profile, each `--param NAME=VALUE`, `--`, then
/// `/usr/bin/env HOME=<job home>`, the program and its arguments. No shell stands between the
/// profile and the program: the helper sets the run's limits, and `env` sets the job's `HOME`
/// (the helper sets its own).
pub struct Parsed {
    pub params: Vec<(String, String)>,
    pub profile: String,
    /// The `HOME` the job gets, as `env` sets it.
    pub home: String,
    /// The program and its arguments.
    pub tail: Vec<String>,
}

pub fn parse(argv: &[String]) -> Parsed {
    assert_eq!(
        argv[0],
        env!("CARGO_BIN_EXE_js-agent"),
        "nothing but the agent runs before the profile"
    );
    assert_eq!(argv[1], "__job-start");
    assert_eq!(argv[2], "--profile");
    let profile = argv[3].clone();
    let mut params = Vec::new();
    let mut at = 4;
    while argv[at] == "--param" {
        let (name, value) = argv[at + 1].split_once('=').unwrap();
        params.push((name.to_owned(), value.to_owned()));
        at += 2;
    }
    assert_eq!(argv[at], "--");
    let at = at + 1;
    assert_eq!(argv[at], "/usr/bin/env", "env sets the job's HOME");
    let home = argv[at + 1].strip_prefix("HOME=").unwrap().to_owned();
    Parsed {
        params,
        profile,
        home,
        tail: argv[at + 2..].to_vec(),
    }
}
