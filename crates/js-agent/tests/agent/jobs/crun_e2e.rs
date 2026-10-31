//! The agent started with `--runtime crun`, over a node made of directories and a stand-in
//! `crun`: the start check passes on stand-in files, the runtime opened is `CrunRuntime`, and
//! a job a real lease server queues is created, started, polled and reported through it. The
//! stand-in is a shell script that holds the container until the test creates a file and then
//! exits 0, so nothing waits for a clock: the stand-in writes a file when the container is up,
//! and the test looks for it between heartbeats (a bounded number of them, so a stand-in
//! that died fails the test at once) and creates the file that lets it end. Whether a real `crun` honours the bundle is the
//! `crun_runtime` target's.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use clap::Parser as _;
use js_agent::cli_runtime::Settings;
use js_agent::crun::{
    CgroupSettings, CgroupSource, Hooks, HostClaim, IdSources, Seams, oci_config, prepare_store,
};
use js_agent::crun_host::{CrunHost, start};
use js_agent::integrity::HostView;
use js_agent::lifecycle::{
    DirLayout, Found, Local, Prepared, RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket,
};
use js_agent::quota::ScratchQuota;
use js_agent::{Cli, FileReader, Handle, Machine, Platform, RuntimeBuild, SharedClock, System};
use js_agent_wire::LogTail;
use js_contain::subids::IdFiles;
use js_testkit::dir::TestDir;
use serde_json::json;

use super::fake::FakeMachine;
use crate::crun_host::Fixture;
use crate::server::{Agent, POOL_RUNS_TOML, Redirect, Server};
use crate::support::{Host, code, linux_host};

/// Held by a test while it opens runtimes or runs an agent on the node: the backend makes
/// its helper directories in the node's store, which two openings at once would fight over.
static NODE_IN_USE: Mutex<()> = Mutex::new(());

/// The first host uid and gid of the subordinate ranges of the node's job user.
const SUB: u32 = 100_000;

/// A wait no test comes to the end of.
const LONG: Duration = Duration::from_secs(3600);

/// The digest of the image in the node's store.
fn digest() -> String {
    format!("sha256:{}", "ab".repeat(32))
}

/// The stand-in `crun`. `run` says it is up in a file and holds until a file is made.
const CRUN: &str = r#"#!/bin/sh
MOUNT=@MOUNT@
DIR=@DIR@
echo "$@" >> "$DIR/calls"
root=; cmd=; rest=
while [ $# -gt 0 ]; do
  case "$1" in
    --root) root=$2; shift 2;;
    --log|--log-format|--pid-file|--console-socket|--signal|--bundle|-b) shift 2;;
    -*) shift;;
    *) if [ -z "$cmd" ]; then cmd=$1; else rest="$rest $1"; fi; shift;;
  esac
done
set -- $rest
name=$1
cg="$MOUNT/actions/$name"
case "$cmd" in
  run)
    mkdir -p "${root:?}/${name:?}"
    echo $$ > "$root/pid"
    echo up > "$DIR/ready"
    while [ ! -e "$DIR/go" ]; do sleep 0.02; done
    exit 0;;
  kill)
    kill -TERM "$(cat "$root/pid")";;
  delete)
    find "$cg" -maxdepth 1 ! -type d -delete 2>/dev/null
    exit 0;;
  state) echo '{"pid":1}';;
esac
"#;

/// The node: stand-in files of a host that passes the start check, a store with one image,
/// cgroup directories, and the stand-in.
struct Node {
    /// The directory of the node, removed when the test binary ends.
    dir: TestDir,
    host: CrunHost,
    crun: PathBuf,
    state: PathBuf,
    scratch: PathBuf,
    ids: IdFiles,
    tools: PathBuf,
    cgroups: PathBuf,
}

/// The one node of this test binary: the seams are function pointers, so what they read is
/// static.
fn node() -> &'static Node {
    static NODE: OnceLock<Node> = OnceLock::new();
    NODE.get_or_init(|| {
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let fixture = Fixture::with_ids("crun-e2e", uid, gid);
        let dir = fixture.dir.path().to_owned();
        let ids = IdFiles::in_dir(&dir.join("ids"));
        fs::create_dir_all(dir.join("ids")).expect("ids");
        fs::write(&ids.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
        fs::write(&ids.subuid, format!("{uid}:{SUB}:65536\n")).expect("subuid");
        fs::write(&ids.subgid, format!("{uid}:{SUB}:65536\n")).expect("subgid");
        let tools = dir.join("tools");
        fs::create_dir_all(&tools).expect("tools");
        for tool in ["newuidmap", "newgidmap"] {
            let path = tools.join(tool);
            fs::write(&path, b"#!/bin/true\n").expect("tool");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).expect("chmod");
        }
        let cgroups = dir.join("runtime-cgroup");
        for part in ["root", "actions"] {
            fs::create_dir_all(cgroups.join(part)).expect("cgroup");
        }
        for (part, file) in [
            ("root", "cgroup.procs"),
            ("actions", "cgroup.procs"),
            ("actions", "cgroup.threads"),
            ("actions", "cgroup.subtree_control"),
        ] {
            fs::write(cgroups.join(part).join(file), b"").expect("a cgroup file");
        }
        let script = CRUN
            .replace("@MOUNT@", &cgroups.display().to_string())
            .replace("@DIR@", &dir.display().to_string());
        fs::write(&fixture.crun, script).expect("the stand-in");
        fs::set_permissions(&fixture.crun, fs::Permissions::from_mode(0o755)).expect("chmod");
        // The image store is beside the scratch root, and holds one image and its config.
        let store = dir.join("oci");
        let hex = digest()[7..].to_owned();
        fs::create_dir_all(store.join("oci-images").join(&hex).join("rootfs")).expect("image");
        fs::create_dir_all(store.join("oci-configs")).expect("configs");
        let config = format!(
            r#"{{"config":{{"Entrypoint":["/bin/tool"],"Env":["A=1"]}},"rootfs":{{"diff_ids":["sha256:{}"]}}}}"#,
            "0".repeat(64)
        );
        fs::write(
            store.join("oci-configs").join(format!("{hex}.json")),
            config,
        )
        .expect("config");
        Node {
            dir: fixture.dir,
            host: fixture.host,
            crun: fixture.crun,
            state: fixture.state,
            scratch: fixture.scratch,
            ids,
            tools,
            cgroups,
        }
    })
}

/// The seams of the crun runtime over the node: its cgroups, its stand-in files for the start
/// check and its id files; nothing is given away and the clock reads 5000.
fn seams() -> Seams {
    let node = node();
    Seams {
        cgroups: CgroupSource::Fixed(CgroupSettings {
            mount: node.cgroups.clone(),
            root: "/root".to_owned(),
            actions: "/actions".to_owned(),
        }),
        hooks: Hooks {
            chown: |_, _, _| Ok(()),
            now_ms: || 5_000,
            kill_wait: LONG,
            pull_limit: LONG,
        },
        host: || self::node().host.clone(),
        ids: IdSources {
            files: node.ids.clone(),
            tool_path: node.tools.clone().into_os_string(),
            check_search: false,
        },
    }
}

/// The host the agent runs on: a Linux host whose `crun --version` answers. Where a path is
/// on a file system is the first of `devices` that holds it (the state root is on a disk of
/// its own, and so is the scratch root when `scratch_apart`), else the node's.
fn host_on(scratch_apart: bool) -> Host {
    let node = node();
    let mut host = linux_host();
    let line = format!("{} --version", node.crun.display());
    host.commands = host
        .commands
        .with(&line, Ok("crun version 1.14.1\ncommit: abc\n"));
    host.files = host.files.with_device_under(&node.state, 8);
    if scratch_apart {
        host.files = host.files.with_device_under(&node.scratch, 9);
    }
    host.files = host.files.with_device_under(node.dir.path(), 7);
    host
}

/// The node's host: everything but the state root on one file system.
fn host() -> Host {
    host_on(false)
}

/// The node's host with the scratch root a mount of its own: the directory above it, where
/// the image store would be, is on another file system.
fn host_with_scratch_mounted() -> Host {
    host_on(true)
}

/// The real [`System`] for the clock, the runtime and the start check, over the node; the
/// view of the host, the cgroup files and the quota are the fake machine's, as a test that is
/// not root cannot read the namespaces of process 1.
struct CrunMachine {
    system: System,
    fake: FakeMachine,
    /// The grace of every stop the runtime was asked for.
    stops: Arc<Mutex<Vec<Duration>>>,
}

impl CrunMachine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            system: System::new().with_crun(seams()),
            fake: FakeMachine::new(),
            stops: Arc::default(),
        })
    }
}

/// A runtime that notes the grace of each stop and leaves everything to the one it wraps.
struct Noting {
    inner: Box<dyn Runtime>,
    stops: Arc<Mutex<Vec<Duration>>>,
}

impl Runtime for Noting {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.inner.prepare(image, waited)
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.inner.create_and_start(spec)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.inner.poll(id)
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.stops.lock().expect("stops").push(grace);
        self.inner.stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.inner.remove(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.inner.list_labelled()
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        self.inner.log_tail(id)
    }

    fn supports_gpus(&self) -> bool {
        self.inner.supports_gpus()
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        self.inner.cgroup(id)
    }
}

impl Machine for CrunMachine {
    fn clock(&self) -> SharedClock {
        self.system.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        let (inner, warnings) = self.system.runtime(build)?;
        let stops = self.stops.clone();
        Ok((Box::new(Noting { inner, stops }), warnings))
    }

    fn view(&self) -> Box<dyn HostView> {
        self.fake.view()
    }

    fn cgroup_files(&self) -> Box<dyn FileReader> {
        self.fake.cgroup_files()
    }

    fn scratch_quota(&self, root: &Path) -> Arc<dyn ScratchQuota> {
        self.fake.scratch_quota(root)
    }

    fn crun_host(&self) -> CrunHost {
        self.system.crun_host()
    }
}

/// The checked settings of the crun runtime for the node, placed beside its scratch root.
fn crun_settings() -> Settings {
    let node = node();
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    Settings::for_crun(node.crun.clone(), Fixture::USER, (uid, gid), Vec::new())
        .placed_beside(&node.scratch, None)
}

/// Asserts that settings for crun are what the engines' flags are to the rest of the agent:
/// no `--container-bin` is needed to have them, the image store is the `oci` directory beside
/// the scratch root (not inside it, where the start-up sweep would remove it), the delegated
/// cgroup is unknown until the runtime records it, and a scratch root without a parent keeps it
/// at the top, and settings of an engine are not touched. It catches settings that put the store inside the sweep's reach,
/// or read an actions cgroup before there is one.
#[test]
fn crun_settings_place_the_store_beside_the_scratch_root() {
    let node = node();
    let settings = crun_settings();
    let plan = settings.crun().expect("crun settings");
    assert_eq!(plan.store, node.dir.path().join("oci"));
    assert_eq!(plan.crun, node.crun);
    assert_eq!(settings.actions_cgroup(), None);
    plan.delegated("/actions");
    plan.delegated("/other");
    assert_eq!(settings.actions_cgroup(), Some("/actions"));
    // The farm unit's layout: the scratch file system is mounted at the directory above the
    // scratch root `runs`, and the store and the claim files are in that directory.
    let farm = Settings::for_crun("/usr/bin/crun".into(), "jsjob", (1, 1), Vec::new())
        .placed_beside(Path::new("/var/lib/js-agent/scratch/runs"), None);
    let farm = farm.crun().expect("crun settings");
    assert_eq!(farm.store, Path::new("/var/lib/js-agent/scratch/oci"));
    assert_eq!(farm.claim_dir, Path::new("/var/lib/js-agent/scratch"));
    // A scratch root with no parent keeps the store beside it, at the top.
    let top = Settings::for_crun("/usr/bin/crun".into(), "jsjob", (1, 1), Vec::new())
        .placed_beside(Path::new("/"), None);
    assert_eq!(
        top.crun().map(|plan| plan.store.clone()),
        Some("/oci".into())
    );
    // Settings of an engine are left as they are.
    let engine = Settings::new(Path::new("/bin/sh"), None, None).expect("an executable");
    let placed = engine.clone().placed_beside(Path::new("/x/scratch"), None);
    assert!(placed.crun().is_none());
    assert_eq!(placed, engine);
}

/// The command line of a crun agent on the node, less `without`.
fn crun_cli(without: &str) -> Cli {
    let node = node();
    let (crun, state, scratch) = (
        node.crun.display().to_string(),
        node.state.display().to_string(),
        node.scratch.display().to_string(),
    );
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend(["--runtime", "crun", "--job-user", Fixture::USER]);
    args.extend(["--crun-bin", &crun]);
    if without != "--state-root" {
        args.extend(["--state-root", &state]);
    }
    args.extend(["--scratch-root", &scratch]);
    Cli::try_parse_from(args).expect("a command line")
}

/// Asserts that the job flags of a crun agent are checked as a container agent's are, with
/// the layout the job user needs: with settings for crun no `--container-bin` is needed, the
/// scratch root may be `0711` (the engines' check would refuse it), the settings come back
/// placed beside it, and a missing state root is refused naming `--runtime crun`. It catches
/// an agent that reads the flags of a crun agent with the rules of the engines.
#[test]
fn the_job_flags_of_a_crun_agent_are_read_with_the_searchable_layout() {
    let local = Local::from_cli_crun(&crun_cli(""), crun_settings()).expect("the flags pass");
    let settings = local.container.as_ref().expect("containers are enabled");
    assert_eq!(
        settings.crun().map(|plan| plan.store.clone()),
        Some(node().dir.path().join("oci"))
    );
    let scratch = local.scratch.as_ref().expect("a scratch root");
    assert_eq!(scratch.layout(), DirLayout::Searchable);

    let engine = Local::from_cli(&crun_cli("")).expect_err("0711 is not private");
    assert!(engine.starts_with("--scratch-root: "), "{engine}");

    let why = Local::from_cli_crun(&crun_cli("--state-root"), crun_settings())
        .expect_err("no state root");
    assert!(
        why.starts_with("--state-root: required with --runtime crun"),
        "{why}"
    );
}

/// Asserts that the machine opens the crun runtime for crun settings and records what the
/// checks need of it: the runtime opens on the node (its cgroups are given and its store is
/// read), the job users the post-job checks look for are the job user's subordinate range,
/// and the actions cgroup the usage is sampled from is the delegated one. The node has no
/// engine, so an engine runtime could not have opened. It catches a machine that builds the
/// engine runtime for `--runtime crun`.
#[test]
fn the_system_opens_the_crun_runtime_for_crun_settings() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let settings = crun_settings();
    let platform = Platform {
        os: "linux".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    };
    let build = RuntimeBuild {
        settings: &settings,
        host_id: "h1",
        platform: &platform,
        stop_grace: Duration::from_secs(1),
    };
    let machine = System::new().with_crun(seams());
    let opened = machine.runtime(&build);
    let warnings = opened.map(|(_, warnings)| warnings).expect("it opens");
    assert!(
        warnings.iter().any(|w| w.contains("--image-allow")),
        "{warnings:?}"
    );
    assert_eq!(settings.actions_cgroup(), Some("/actions"));
    let range = settings.uid().range().expect("the mapping is recorded");
    assert!(range.contains(SUB) && range.contains(SUB + 65_535) && !range.contains(SUB + 65_536));
}

/// A server, an agent started with `--runtime crun` over the node, and a lease on its host.
struct Rig {
    server: Server,
    agent: Agent,
    worker: Handle,
    lease: String,
    machine: Arc<CrunMachine>,
    flags: Vec<String>,
    address: Arc<Mutex<String>>,
}

/// The flags of a crun agent of the node with host id `host_id`, and `extra`.
fn crun_flags(host_id: &str, extra: &[&str]) -> Vec<String> {
    let node = node();
    let mut flags: Vec<String> = [
        "--host-id",
        host_id,
        "--runtime",
        "crun",
        "--job-user",
        Fixture::USER,
        "--crun-bin",
        &node.crun.display().to_string(),
        "--state-root",
        &node.state.display().to_string(),
        "--scratch-root",
        &node.scratch.display().to_string(),
    ]
    .map(str::to_owned)
    .into();
    flags.extend(extra.iter().map(|flag| (*flag).to_owned()));
    flags
}

impl Rig {
    /// Starts the server and the agent, waits until the agent has said hello, and takes a
    /// lease on its host.
    fn new(name: &str, extra: &[&str]) -> Self {
        let node = node();
        for signal in ["ready", "go", "calls"] {
            let _ = fs::remove_file(node.dir.path().join(signal));
        }
        let server = Server::start_pool(name, &[], POOL_RUNS_TOML);
        let address = Arc::new(Mutex::new(server.url()));
        let flags = crun_flags("h1", extra);
        let machine = CrunMachine::new();
        let agent = Self::spawn(&server, &address, &flags, &machine);
        let worker = match agent.try_worker() {
            Ok(worker) => worker,
            Err(()) => panic!(
                "the agent ended before its worker started: {}",
                agent.finish().err
            ),
        };
        assert_eq!(agent.next_wait(), Duration::from_secs(5), "the hello");
        let body = json!({
            "class": "mac",
            "resources": { "cpu_millis": 1000, "memory_bytes": 1024 },
            "capabilities": { "os": "linux" },
            "max_wall_seconds": 3600,
        });
        let granted = server.post_body("/v1/leases", &body);
        assert_eq!(granted.status, 201, "{}", granted.body);
        let lease = granted.body["id"].as_str().unwrap().to_owned();
        Self {
            server,
            agent,
            worker,
            lease,
            machine,
            flags,
            address,
        }
    }

    /// An agent with `flags`, over a machine that is the node's.
    fn spawn(
        server: &Server,
        address: &Arc<Mutex<String>>,
        flags: &[String],
        machine: &Arc<CrunMachine>,
    ) -> Agent {
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        Agent::spawn_machine(
            server.url(),
            server.join_file(),
            &flags,
            host,
            Redirect(address.clone()),
            machine.clone(),
        )
    }

    /// A second agent with `flags` against the same server, run to its end.
    fn second(&self, flags: &[String]) -> crate::support::Ran {
        Self::spawn(&self.server, &self.address, flags, &CrunMachine::new()).finish()
    }

    /// One heartbeat of the agent, and the worker done with the reply to it.
    fn beat(&self) {
        self.agent.elapse();
        assert_eq!(self.agent.next_wait(), Duration::from_secs(5));
        assert!(self.worker.settle());
    }

    /// Queues a run of the node's image; returns its id.
    fn queued(&self) -> String {
        let queued = self.server.post_body(
            &format!("/v1/leases/{}/runs", self.lease),
            &json!({
                "image": format!("registry.example/job:1@{}", digest()),
                "args": ["true"],
                "limits": { "cpu_millis": 4000, "memory_bytes": 1048576 },
            }),
        );
        assert_eq!(queued.status, 202, "{}", queued.body);
        queued.body["run_id"].as_str().unwrap().to_owned()
    }

    /// What the listing says of `run`: its state, and its outcome when it has one.
    fn view(&self, run: &str) -> (String, Option<String>) {
        let listing = self.server.get(&format!("/v1/leases/{}/runs", self.lease));
        let runs = listing["runs"].as_array().unwrap().clone();
        let entry = runs.iter().find(|entry| entry["run_id"] == run);
        let entry = entry.unwrap_or_else(|| panic!("{run} is not listed: {runs:?}"));
        (
            entry["state"].as_str().unwrap().to_owned(),
            entry["outcome"].as_str().map(str::to_owned),
        )
    }

    /// Beats until `done` holds of what the listing says of `run`. The heartbeats are the
    /// progress events; a bound on their number keeps a regression from looping.
    fn until(&self, run: &str, done: impl Fn(&(String, Option<String>)) -> bool) {
        let mut seen = self.view(run);
        for _ in 0..100 {
            if done(&seen) {
                return;
            }
            self.beat();
            seen = self.view(run);
        }
        panic!("{run} never got there: {seen:?}");
    }

    /// Beats until the stand-in has said it is up, and the run is listed running.
    fn until_up(&self, run: &str) {
        let ready = node().dir.path().join("ready");
        for _ in 0..100 {
            let said = fs::read_to_string(&ready).unwrap_or_default();
            if said.trim() == "up" {
                return self.until(run, |seen| seen.0 == "running");
            }
            self.beat();
        }
        panic!("the stand-in never said it was up");
    }
}

/// Asserts the whole path with `--runtime crun`: the agent passes its start check on the
/// node's stand-in files (it does not exit 2 saying crun jobs are unavailable), registers, takes
/// a run the server queues, starts it through `crun run` of the stand-in with the run's
/// container name, reports it running while the stand-in holds it, and reports it exited and
/// succeeded once the stand-in ends; the agent then stops with status 0. The image store is
/// intact after the agent's start-up sweep. While the agent runs, a second agent with the same
/// host id, and one with another id on the same scratch root, are each refused with status 2
/// naming what they share, and both claims are free again after the agent ends. It catches an
/// agent that checks the host and then runs no crun job, a run that never reaches `crun`, a
/// result that is lost on the way back, and two agents that would sweep each other.
#[test]
fn an_agent_started_with_runtime_crun_runs_a_job_through_crun() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let node = node();
    let rig = Rig::new("jobs_crun_e2e", &[]);
    // The agent's start-up sweep of the scratch root leaves the image store, which is beside it.
    let image = node.dir.path().join("oci/oci-images").join(&digest()[7..]);
    assert!(
        image.join("rootfs").is_dir(),
        "the image store survived the start"
    );
    let run = rig.queued();
    rig.until_up(&run);
    let calls = fs::read_to_string(node.dir.path().join("calls")).expect("the stand-in's calls");
    let started = calls
        .lines()
        .any(|line| line.contains(" run ") && line.contains(&run));
    assert!(started, "no `crun run` for {run} in\n{calls}");

    fs::write(node.dir.path().join("go"), "go").expect("let the container end");
    rig.until(&run, |seen| seen.1.is_some());
    assert_eq!(
        rig.view(&run),
        ("exited".to_owned(), Some("succeeded".to_owned()))
    );

    let same_id = rig.second(&rig.flags);
    assert_eq!(same_id.status(), code(2), "{}", same_id.err);
    assert!(
        same_id.err.contains("--host-id h1: another crun agent"),
        "{}",
        same_id.err
    );
    let other_id = rig.second(&crun_flags("h2", &[]));
    assert_eq!(other_id.status(), code(2), "{}", other_id.err);
    assert!(
        other_id.err.contains("--scratch-root ") && other_id.err.contains("another crun agent"),
        "{}",
        other_id.err
    );
    assert!(
        other_id.err.contains(&std::process::id().to_string()),
        "the holder is named: {}",
        other_id.err
    );
    let held = HostClaim::take(node.dir.path(), "h1").expect_err("the id is held");
    assert!(held.to_string().contains(&std::process::id().to_string()));
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("does not start jobs"), "{}", ran.err);
    // The claims go with the agent: a restart finds them free.
    let dir = node.dir.path();
    HostClaim::take(dir, "h1").expect("the id is free after the agent ended");
    HostClaim::scratch_root(dir, &node.scratch).expect("the scratch root is free too");
}

/// Asserts that `--stop-grace` is what the runtime is told to wait before it kills: an agent
/// started with `--stop-grace 7 --drain-wait 0` that holds a running job, asked to stop,
/// calls `stop` with exactly seven seconds (nothing waits for them: the stand-in ends on the
/// SIGTERM), and the stand-in is sent the signal. It catches a wiring that gives the runtime
/// a grace of its own.
#[test]
fn the_stop_grace_reaches_the_crun_runtime() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new(
        "jobs_crun_stop",
        &["--stop-grace", "7", "--drain-wait", "0"],
    );
    let run = rig.queued();
    rig.until_up(&run);
    rig.agent.signal();
    rig.agent.next_wait();
    assert!(rig.worker.settle());
    rig.agent.elapse();
    rig.agent.next_wait();
    assert!(rig.worker.settle());
    rig.agent.elapse();
    let stops = rig.machine.stops.clone();
    let ran = rig.agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let stops = stops.lock().expect("stops");
    assert!(!stops.is_empty(), "the job was stopped through the runtime");
    assert!(
        stops.iter().all(|grace| *grace == Duration::from_secs(7)),
        "{stops:?}"
    );
    let calls = fs::read_to_string(node().dir.path().join("calls")).expect("calls");
    assert!(calls.lines().any(|line| line.contains(" kill ")), "{calls}");
}

/// Asserts the layout the store needs: with the scratch root on one file system and the
/// directory above it, where the image store and the leases' `/work` and home are kept, on
/// another, the agent exits 2 naming both paths; and `--scratch-quota`, which would bound
/// only `/scratch`, exits 2 naming the flag. It catches an agent that lets a job fill the
/// root file system, or a quota that is believed to cover a lease's `/work`.
#[test]
fn a_store_on_another_file_system_and_a_scratch_quota_are_refused() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let node = node();
    let server = Server::start_pool("jobs_crun_fs", &[], POOL_RUNS_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let run = |host: fn() -> Host, extra: &[&str]| {
        let flags = crun_flags("h-fs", extra);
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        Agent::spawn_machine(
            server.url(),
            server.join_file(),
            &flags,
            host,
            Redirect(address.clone()),
            CrunMachine::new(),
        )
        .finish()
    };
    let apart = run(host_with_scratch_mounted, &[]);
    assert_eq!(apart.status(), code(2), "{}", apart.err);
    assert!(
        apart.err.contains("different file systems")
            && apart.err.contains(&node.scratch.display().to_string())
            && apart.err.contains(&node.dir.path().display().to_string()),
        "{}",
        apart.err
    );
    let quota = run(host, &["--scratch-quota"]);
    assert_eq!(quota.status(), code(2), "{}", quota.err);
    assert!(
        quota
            .err
            .contains("--scratch-quota: not accepted with --runtime crun"),
        "{}",
        quota.err
    );
}

/// Asserts that the directory above the scratch root and the store in it are checked before
/// a claim file or an image goes there: a good parent gets its store made; a store that is a
/// link, a parent that a group can write, a parent owned by someone else and a parent that is
/// not there are each refused. It catches a store that is a link planted to put the images
/// somewhere else, or a claim file made where another user can replace it.
#[test]
fn the_store_and_the_directory_above_the_scratch_root_must_be_the_agents_own() {
    use std::os::unix::fs::symlink;
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "crun-store");
    let root = dir.path().to_owned();
    let mode = |path: &Path, mode| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("mode");
    };

    // The farm unit's shape: the directory above `runs` holds the store.
    let good = root.join("scratch");
    fs::create_dir(&good).expect("parent");
    mode(&good, 0o711);
    fs::create_dir(good.join("runs")).expect("the scratch root");
    mode(&good.join("runs"), 0o711);
    prepare_store(&good, &good.join("oci")).expect("a good parent");
    assert!(good.join("oci").is_dir());
    prepare_store(&good, &good.join("oci")).expect("and again, with the store there");

    let linked = root.join("linked");
    fs::create_dir(&linked).expect("parent");
    mode(&linked, 0o755);
    symlink(&good, linked.join("oci")).expect("a link");
    let why = prepare_store(&linked, &linked.join("oci")).expect_err("a link");
    assert!(why.contains("not a link"), "{why}");

    let writable = root.join("writable");
    fs::create_dir(&writable).expect("parent");
    mode(&writable, 0o775);
    let why = prepare_store(&writable, &writable.join("oci")).expect_err("group-writable");
    assert!(why.contains("must be a directory"), "{why}");
    assert!(!writable.join("oci").exists(), "nothing is made in it");

    let why = prepare_store(&root.join("missing"), &root.join("missing/oci")).expect_err("gone");
    assert!(why.contains("missing"), "{why}");

    // Not the agent's own (or, for a root test run, a directory nothing can be made in).
    let proc = Path::new("/proc");
    prepare_store(proc, &proc.join("js-oci-test")).expect_err("not the agent's");
}

/// Asserts that the ids a crun is run as are the job user's that the start check found, not
/// anything made up by the test: the check, run on the node's stand-in files, finds the test
/// user's ids; the settings and the backend's configuration built from it carry them.
#[test]
fn the_backend_runs_crun_as_the_ids_the_start_check_found() {
    let node = node();
    let ready = start(&crun_cli(""), &node.host, &host().commands).expect("the host passes");
    let ids = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    assert_eq!(ready.job_ids, ids);
    let settings = Settings::for_crun(ready.crun, &ready.job_user, ready.job_ids, Vec::new());
    let plan = settings.crun().expect("crun settings");
    let config = oci_config(&plan.crun, plan.job_ids);
    assert_eq!(config.job_user, Some(ids));
    assert_eq!(config.runtime, node.crun);
}

/// Asserts the claim of a host id: a second claim of the same id in the same directory is
/// refused, naming the process that holds the first; another id is free; and once the first is
/// dropped the id can be claimed again (the system drops the lock with a process that ends, so
/// a restarted agent starts). It catches two agents of one machine sharing the default host id
/// and so sweeping each other's leases.
#[test]
fn a_host_id_is_held_by_one_agent_at_a_time() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "crun-claim");
    let first = HostClaim::take(dir.path(), "host-a").expect("the first claim");
    let why = HostClaim::take(dir.path(), "host-a").expect_err("held");
    assert!(
        why.to_string()
            .contains(&format!("{:?}", std::process::id().to_string())),
        "{why}"
    );
    let other = HostClaim::take(dir.path(), "host-b").expect("another id is free");
    drop(first);
    HostClaim::take(dir.path(), "host-a").expect("free once dropped");
    drop(other);
}

/// Asserts the claim of a host id cannot be taken in a directory that is not there, and says
/// where. It catches a claim that is taken silently when nothing is locked.
#[test]
fn a_claim_in_a_missing_directory_is_an_error_naming_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "crun-claim-missing");
    let missing = dir.path().join("not-there");
    let why = HostClaim::take(&missing, "host-a").expect_err("no directory");
    assert!(why.to_string().contains("not-there"), "{why}");
}

/// Asserts how the agent's cgroups are set up when the runtime opens, on a tree of files
/// that stands for a cgroup version 2 mount. An agent in the leaf `<unit>/supervisor` has the
/// unit as its root; an agent in the unit's own cgroup is moved into that leaf (its process
/// is written to the leaf's `cgroup.procs`). Either way the leases go in `<unit>/actions`
/// and cpu, memory and pids are enabled in the unit's cgroup and in `actions`. Each way the
/// set-up can fail is a refusal naming the cgroups: a unit that offers too few controllers,
/// a file that cannot be written in either cgroup, `actions` that cannot be made, a file that
/// does not name a cgroup, and one that is not there. It catches a runtime given the wrong
/// cgroup, or an agent that starts without cgroups it can lease from.
#[test]
fn the_agents_cgroups_are_set_up_when_the_runtime_opens() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let node = node();
    let dir = node.dir.path();
    let platform = Platform {
        os: "linux".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    };
    // A fresh tree for each case: a unit that offers `controllers` and has the process
    // 4242 in it, and the agent's cgroup.
    let tree = |name: &str, controllers: &str, own: &str| {
        let mount = dir.join(name);
        fs::create_dir_all(mount.join("unit")).expect("the unit's cgroup");
        fs::write(mount.join("cgroup.controllers"), controllers).expect("controllers");
        fs::write(mount.join("unit/cgroup.controllers"), controllers).expect("controllers");
        fs::write(mount.join("unit/cgroup.procs"), "4242\n").expect("procs");
        let file = dir.join(format!("{name}-own"));
        fs::write(&file, own).expect("own cgroup");
        (mount, file)
    };
    let open = |mount: &Path, own: &Path, settings: &Settings| {
        let build = RuntimeBuild {
            settings,
            host_id: "h1",
            platform: &platform,
            stop_grace: Duration::from_secs(1),
        };
        let cgroups = CgroupSource::Delegate {
            mount: mount.to_owned(),
            own: own.to_owned(),
        };
        let machine = System::new().with_crun(Seams { cgroups, ..seams() });
        machine.runtime(&build).map(|_| ())
    };
    let refused = |mount: &Path, own: &Path| open(mount, own, &crun_settings()).unwrap_err();
    let enabled = |mount: &Path, cgroup: &str| {
        fs::read_to_string(mount.join(cgroup).join("cgroup.subtree_control")).expect("enabled")
    };

    // The agent is in the leaf `supervisor`: the unit is the root.
    let (mount, own) = tree("cg-leaf", "cpu memory pids\n", "0::/unit/supervisor\n");
    let settings = crun_settings();
    open(&mount, &own, &settings).expect("it opens");
    assert_eq!(settings.actions_cgroup(), Some("/unit/actions"));
    for cgroup in ["unit", "unit/actions"] {
        assert_eq!(enabled(&mount, cgroup), "+cpu +memory +pids", "{cgroup}");
    }
    // The agent is in the unit's own cgroup: it is moved into the leaf.
    let (mount, own) = tree("cg-moved", "cpu memory pids\n", "0::/unit\n");
    let settings = crun_settings();
    open(&mount, &own, &settings).expect("it opens");
    assert_eq!(settings.actions_cgroup(), Some("/unit/actions"));
    let moved = fs::read_to_string(mount.join("unit/supervisor/cgroup.procs"));
    assert_eq!(moved.expect("the process was written").trim(), "4242");
    assert_eq!(enabled(&mount, "unit"), "+cpu +memory +pids");

    let (mount, own) = tree("cg-few", "cpu memory\n", "0::/unit/supervisor\n");
    assert!(refused(&mount, &own).contains("does not offer pids"));
    let (mount, own) = tree(
        "cg-closed-unit",
        "cpu memory pids\n",
        "0::/unit/supervisor\n",
    );
    fs::create_dir(mount.join("unit/cgroup.subtree_control")).expect("a directory");
    assert!(refused(&mount, &own).contains("enable controllers in"));
    let (mount, own) = tree(
        "cg-closed-actions",
        "cpu memory pids\n",
        "0::/unit/supervisor\n",
    );
    fs::create_dir_all(mount.join("unit/actions/cgroup.subtree_control")).expect("a directory");
    assert!(refused(&mount, &own).contains("enable controllers in"));
    let (mount, own) = tree("cg-v1", "cpu memory pids\n", "1:name=x:/unit\n");
    assert!(refused(&mount, &own).contains("no cgroup v2"));
    let (mount, own) = tree("cg-nomake", "cpu memory pids\n", "0::/unit/supervisor\n");
    fs::write(mount.join("unit/actions"), "a file").expect("a file where actions goes");
    assert!(refused(&mount, &own).contains("make"));
    let why = refused(&mount, &dir.join("no-such-file"));
    assert!(why.starts_with("the cgroups: read "), "{why}");
}

/// Asserts that an agent whose cgroups cannot be set up ends with status 2 before it
/// registers, naming the cgroups. It catches an agent that starts on a unit it cannot lease
/// from.
#[test]
fn an_agent_whose_cgroups_cannot_be_set_up_exits_2() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let node = node();
    let mount = node.dir.path().join("cg-agent");
    fs::create_dir_all(&mount).expect("the mount");
    let own = node.dir.path().join("cg-agent-own");
    fs::write(&own, "1:name=x:/unit\n").expect("own cgroup");
    let server = Server::start_pool("jobs_crun_cgroups", &[], POOL_RUNS_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let (crun, state, scratch) = (
        node.crun.display().to_string(),
        node.state.display().to_string(),
        node.scratch.display().to_string(),
    );
    let flags = [
        "--host-id",
        "h-cgroups",
        "--runtime",
        "crun",
        "--job-user",
        Fixture::USER,
        "--crun-bin",
        &crun,
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ];
    let cgroups = CgroupSource::Delegate { mount, own };
    let machine = Arc::new(CrunMachine {
        system: System::new().with_crun(Seams { cgroups, ..seams() }),
        fake: FakeMachine::new(),
        stops: Arc::default(),
    });
    let ran = Agent::spawn_machine(
        server.url(),
        server.join_file(),
        &flags,
        host,
        Redirect(address),
        machine,
    )
    .finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--runtime crun: the cgroups"),
        "{}",
        ran.err
    );
}
