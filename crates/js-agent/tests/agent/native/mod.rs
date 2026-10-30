//! Native jobs: the runtime that starts a program as a user of the root helper, under a
//! Seatbelt profile, and the lifecycle that drives it. The helper is a fake that speaks the
//! helper's real protocol on a socket (`fake_helper`), so everything but what the verbs do
//! to a machine runs as in the agent. On a Mac, one test module applies the runtime's
//! `sandbox-exec` command line for real, as the user running the tests (`mac`).

mod agent;
mod fake_helper;
mod journal;
mod lifecycle;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod mac_agent;
mod runtime;
mod settings;
mod worker;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};

use crate::support::Scratch;
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
        net: Net::Loopback,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![AbsPath::parse("/srv/js-state").unwrap()],
        read_model: ReadModel::Open,
        journal: fake
            .homes()
            .parent()
            .expect("the homes are below the test's directory")
            .join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
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
        let rt = NativeRuntime::open(fake.client(), config, TERM).unwrap();
        Self { fake, rt, dir }
    }

    /// A run of `native`, with its scratch directory made as the lifecycle makes it.
    pub fn spec(&self, run_id: &str, native: NativeSpec) -> RunSpec {
        let scratch = self.dir.path("scratch").join(run_id);
        fs::create_dir_all(&scratch).unwrap();
        RunSpec {
            run_id: run_id.to_owned(),
            body: Body::Native(native),
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            scratch,
            files: None,
            slot: 0,
            isolation: Isolation::Contained,
            labels: BTreeMap::new(),
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
    }
}

/// The script of the shell that sets a run's limits and becomes its command.
pub const WRAPPER: &str = "ulimit -f \"$1\" && ulimit -u \"$2\" && shift 2 && exec \"$@\"";

/// The command line the runtime sent the helper, taken apart: `sandbox-exec`, the `-D`
/// parameters, `-p` and the profile, then the shell that sets the run's limits inside the
/// profile, then the program and its arguments.
pub struct Parsed {
    /// The limits the shell wrapper sets: the file size in 1024-byte blocks and the most
    /// processes.
    pub limits: (String, String),
    pub params: Vec<(String, String)>,
    pub profile: String,
    pub tail: Vec<String>,
}

/// The shell that sets the limits: it is the profile's first program, so nothing the job's
/// environment steers (a shell reads `SHELLOPTS`, `PS4`, `ENV` and imported functions
/// before its first command) runs outside the profile.
pub const LIMITER: &str = "/bin/sh";

pub fn parse(argv: &[String]) -> Parsed {
    assert_eq!(argv[0], "/usr/bin/sandbox-exec", "nothing runs before the profile");
    let mut params = Vec::new();
    let mut at = 1;
    while argv[at] == "-D" {
        let (name, value) = argv[at + 1].split_once('=').unwrap();
        params.push((name.to_owned(), value.to_owned()));
        at += 2;
    }
    assert_eq!(argv[at], "-p");
    let profile = argv[at + 1].clone();
    let rest = &argv[at + 2..];
    assert_eq!(&rest[..4], [LIMITER, "-c", WRAPPER, "js-native"]);
    Parsed {
        limits: (rest[4].clone(), rest[5].clone()),
        params,
        profile,
        tail: rest[6..].to_vec(),
    }
}
