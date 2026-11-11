//! The hello of a whole `--runtime crun` agent: `gpu-passthrough` is reported by an agent
//! given a device list and by no other, `host-jobs` by an agent given `--allow-host-jobs` and
//! by no other, and an operator cannot claim either. Each run of the agent
//! has a host of its own: the stand-in files of the crun start check in a directory of the
//! test (`Fixture`), and the fake machine's runtime, so nothing is shared with another test.
//! `/dev/null` stands for a GPU's node: a character device every Linux host has, so the start
//! check reads a real node.

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::crun_host::CrunHost;
use js_agent::integrity::HostView;
use js_agent::lifecycle::Runtime;
use js_agent::quota::ScratchQuota;
use js_agent::usage::UsageClock;
use js_agent::{Backend, FileReader, Handle, Machine, RuntimeBuild, SharedClock, Sleeper, Wake};
use js_agent_wire::{BookedResources, Command, HeartbeatReply, Isolation, JobStartSpec};
use serde_json::Value;

use super::fake::FakeMachine;
use crate::bounds::WakeBudget;
use crate::crun_host::Fixture;
use crate::support::{
    Host, Jobs, Lowest, Ran, Script, ScriptConnector, beat_ok, code, flags, linux_host, refused,
    reply, run_on, welcome,
};

/// A sleeper that lets the worker (taken from the hook) finish what it was given at every
/// wait, and asks for the stop at the `stop_at`th wait.
struct Settle<'a> {
    seen: &'a RefCell<Option<Handle>>,
    calls: usize,
    stop_at: usize,
    budget: WakeBudget,
}

impl Sleeper for Settle<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        if let Some(handle) = self.seen.borrow().as_ref() {
            handle.settle();
        }
        self.calls += 1;
        match self.calls >= self.stop_at {
            true => Wake::Stop,
            false => Wake::Elapsed,
        }
    }
}

/// The fake machine, with the start check of `--runtime crun` reading the test's own host.
struct GpuHost {
    fake: FakeMachine,
    crun: CrunHost,
    /// For each runtime made: whether its crun plan launches host jobs (none for a backend
    /// with no crun plan).
    host_jobs: Mutex<Vec<Option<bool>>>,
}

impl Machine for GpuHost {
    fn clock(&self) -> SharedClock {
        self.fake.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        let plan = match build.backend {
            Backend::Container(settings) => settings.crun().map(|plan| plan.host_jobs),
            Backend::Native(_) => None,
        };
        self.host_jobs.lock().expect("host jobs").push(plan);
        self.fake.runtime(build)
    }

    fn view(&self) -> Box<dyn HostView> {
        self.fake.view()
    }

    fn usage_clock(&self) -> Box<dyn UsageClock> {
        self.fake.usage_clock()
    }

    fn crun_host(&self) -> CrunHost {
        self.crun.clone()
    }

    fn scratch_quota(&self, root: &Path) -> Arc<dyn ScratchQuota> {
        self.fake.scratch_quota(root)
    }

    fn cgroup_files(&self) -> Box<dyn FileReader> {
        self.fake.cgroup_files()
    }
}

/// The host the agent of `fixture` runs on: a Linux host with two GPUs whose `crun --version`
/// answers, whose scratch root has 10 GiB free, and whose state root is on a disk of its own.
fn host_of(fixture: &Fixture) -> Host {
    let mut host = linux_host();
    let version = format!("{} --version", fixture.crun.display());
    host.commands = host
        .commands
        .with(&version, Ok("crun version 1.14.1\ncommit: abc\n"));
    let scratch = fs::canonicalize(&fixture.scratch).expect("the scratch root");
    host.commands = host.commands.with(
        &format!("/bin/df -P -k {}", scratch.display()),
        Ok(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
            /dev/sda1 99999999 100 10485760 11% /\n",
        ),
    );
    host.files = host
        .files
        .with_device_under(&fixture.state, 8)
        .with_device_under(fixture.dir.path(), 7);
    host.euid = rustix::process::geteuid().as_raw();
    host.ancestors_floor = fixture.dir.path().to_owned();
    host
}

/// Runs a crun agent with `extra`, on a host of its own named `name`, until the server
/// refuses its hello; returns what it printed and the hello it sent (`null` for none).
fn crun_hello(name: &str, extra: &[&str]) -> (Ran, Value) {
    let (ran, sent, _) = crun_run(name, extra, vec![refused(403, "host-bound")], 1);
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Runs a crun agent with `extra`, on a host of its own named `name`, answered with
/// `replies` and stopped at its `stop_at`-th wait; returns what it printed, what it sent and
/// its machine.
fn crun_run(
    name: &str,
    extra: &[&str],
    replies: Vec<crate::support::Reply>,
    stop_at: usize,
) -> (Ran, crate::support::Sent, Arc<GpuHost>) {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let fixture = Fixture::with_ids(name, uid, gid);
    let mut owned: Vec<String> = [
        "--host-id",
        "h1",
        "--runtime",
        "crun",
        "--job-user",
        Fixture::USER,
        "--crun-bin",
        &fixture.crun.display().to_string(),
        "--state-root",
        &fixture.state.display().to_string(),
        "--scratch-root",
        &fixture.scratch.display().to_string(),
    ]
    .map(str::to_owned)
    .into();
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(replies);
    let connector = ScriptConnector::of(script);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let mut sleeper = Settle {
        seen: &seen,
        calls: 0,
        stop_at,
        budget: WakeBudget::new(),
    };
    let gpu_host = Arc::new(GpuHost {
        fake: FakeMachine::new(),
        crun: fixture.host.clone(),
        host_jobs: Mutex::new(Vec::new()),
    });
    let machine: Arc<dyn Machine> = gpu_host.clone();
    let jobs: Jobs<'_> = (machine, Some(&hook));
    let host = host_of(&fixture);
    let ran = run_on(
        &flags(&all),
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    (ran, sent, gpu_host)
}

/// Proves a crun agent on a host with two GPUs reports `gpu-passthrough=true` beside its
/// runtime when it is given `--gpu-device`, and does not report it without one; and that
/// `--capability gpu-passthrough=true` is refused naming the flag before a hello is sent.
/// Catches an agent that claims to pass GPUs it has no device list for (its GPU jobs would be
/// refused `gpu-unsupported`), one that does not say it passes them, and a capability an
/// operator could set by hand.
#[test]
fn the_hello_reports_gpu_passthrough_only_with_a_device_list() {
    let (ran, hello) = crun_hello("hello-gpu-listed", &["--gpu-device", "/dev/null"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["gpu-passthrough"], "true", "{hello}");
    assert_eq!(caps["runtime"], "container", "{hello}");
    assert_eq!(hello["shape"]["gpus"], 2, "{hello}");

    let (ran, hello) = crun_hello("hello-gpu-none", &[]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["runtime"], "container", "{hello}");
    assert!(caps.get("gpu-passthrough").is_none(), "{hello}");

    let claimed = ["--capability", "gpu-passthrough=true"];
    let (ran, hello) = crun_hello("hello-gpu-claimed", &claimed);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said = "--capability: gpu-passthrough is detected and cannot be set";
    assert!(ran.err.contains(said), "{}", ran.err);
    assert_eq!(hello, Value::Null);
}

/// Proves a crun agent started with `--allow-host-jobs` takes the flag and reports
/// `host-jobs=true` beside its runtime, and that one started without it reports no
/// `host-jobs`. Catches a crun agent that refuses the flag its runtime honours, and one that
/// reports host jobs it was not allowed to run (the server would taint the host for a job the
/// agent then refuses).
#[test]
fn the_hello_reports_host_jobs_only_with_allow_host_jobs() {
    let (ran, hello) = crun_hello("hello-host-jobs", &["--allow-host-jobs"]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["host-jobs"], "true", "{hello}");
    assert_eq!(caps["runtime"], "container", "{hello}");

    let (ran, hello) = crun_hello("hello-host-jobs-none", &[]);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["runtime"], "container", "{hello}");
    assert!(caps.get("host-jobs").is_none(), "{hello}");
}

/// A heartbeat reply that starts the job `j1` with `isolation = host`.
fn host_start() -> crate::support::Reply {
    let start = JobStartSpec {
        job_id: "j1".to_owned(),
        allocation_id: "alloc-j1".to_owned(),
        image: format!("registry.example/job:1@sha256:{}", "ab".repeat(32)),
        command: vec!["--go".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Isolation::Host,
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
    };
    let beat = HeartbeatReply {
        commands: vec![Command::JobStart(Box::new(start))],
        ..HeartbeatReply::default()
    };
    reply(200, serde_json::to_value(beat).expect("a reply"))
}

/// Proves the whole path of `--allow-host-jobs` through a crun agent: the flag reaches the
/// crun plan the runtime is opened from (`host_jobs` set) and the lifecycle, so a host job
/// start delivered in a heartbeat reply is created in the runtime as a host job. Without the
/// flag the same start reaches no runtime, and the plan says host jobs are off. Catches a flag
/// that is reported in the hello but never reaches the lifecycle or the runtime, so an agent
/// that advertises host jobs refuses every one it is sent.
#[test]
fn a_host_job_start_reaches_the_runtime_only_with_allow_host_jobs() {
    let replies = || vec![welcome(1), host_start(), beat_ok(), beat_ok()];
    let (ran, _, allowed) = crun_run("host-jobs-run", &["--allow-host-jobs"], replies(), 3);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(*allowed.host_jobs.lock().expect("plans"), [Some(true)]);
    assert_eq!(
        allowed.fake.world.isolations(),
        [("j1".to_owned(), Isolation::Host)]
    );

    let (ran, _, refused) = crun_run("host-jobs-off", &[], replies(), 3);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(*refused.host_jobs.lock().expect("plans"), [Some(false)]);
    assert!(refused.fake.world.isolations().is_empty());
}
