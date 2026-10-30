//! The agent started with `--runtime crun`, over a node made of directories and a stand-in
//! `crun`: the start check passes on stand-in files, the runtime opened is `CrunRuntime`, and
//! a job a real lease server queues is created, started, polled and reported through it. The
//! stand-in is a shell script that holds the container until the test opens a named pipe and
//! then exits 0, so nothing waits for a clock: the test reads a pipe to know the container is
//! up and writes a pipe to let it end. Whether a real `crun` honours the bundle is the
//! `crun_runtime` target's.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use clap::Parser as _;
use js_agent::cli_runtime::Settings;
use js_agent::crun::{CgroupSettings, Hooks, HostClaim, Seams};
use js_agent::crun_host::CrunHost;
use js_agent::integrity::HostView;
use js_agent::lifecycle::{DirLayout, Local, Runtime};
use js_agent::quota::ScratchQuota;
use js_agent::{Cli, FileReader, Machine, Platform, RuntimeBuild, SharedClock, System};
use js_contain::subids::IdFiles;
use js_testkit::dir::TestDir;
use serde_json::json;

use super::fake::FakeMachine;
use crate::crun_host::Fixture;
use crate::server::{Agent, POOL_RUNS_TOML, Redirect, Server};
use crate::support::{Host, code, linux_host};

/// The first host uid and gid of the subordinate ranges of the node's job user.
const SUB: u32 = 100_000;

/// A wait no test comes to the end of.
const LONG: Duration = Duration::from_secs(3600);

/// The digest of the image in the node's store.
fn digest() -> String {
    format!("sha256:{}", "ab".repeat(32))
}

/// The stand-in `crun`. `run` says it is up on a pipe and holds until a pipe is written.
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
    cat "$DIR/gate" > /dev/null
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
        for pipe in ["ready", "gate"] {
            let made = Command::new("mkfifo").arg(dir.join(pipe)).status();
            assert!(made.expect("mkfifo").success());
        }
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
    Seams {
        cgroups: || {
            Ok(CgroupSettings {
                mount: node().cgroups.clone(),
                root: "/root".to_owned(),
                actions: "/actions".to_owned(),
            })
        },
        hooks: Hooks {
            chown: |_, _, _| Ok(()),
            now_ms: || 5_000,
            kill_wait: LONG,
            pull_limit: LONG,
        },
        host: || node().host.clone(),
        configure: |oci| {
            oci.id_files = node().ids.clone();
            oci.check_search = false;
            oci.id_tool_path = Some(node().tools.clone().into_os_string());
        },
    }
}

/// The host the agent runs on: a Linux host whose `crun --version` answers.
fn host() -> Host {
    let mut host = linux_host();
    let line = format!("{} --version", node().crun.display());
    host.commands = host
        .commands
        .with(&line, Ok("crun version 1.14.1\ncommit: abc\n"));
    host
}

/// The real [`System`] for the clock, the runtime and the start check, over the node; the
/// view of the host, the cgroup files and the quota are the fake machine's, as a test that is
/// not root cannot read the namespaces of process 1.
struct CrunMachine {
    system: System,
    fake: FakeMachine,
}

impl Machine for CrunMachine {
    fn clock(&self) -> SharedClock {
        self.system.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        self.system.runtime(build)
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
        .expect("placed")
}

/// Asserts that settings for crun are what the engines' flags are to the rest of the agent:
/// no `--container-bin` is needed to have them, the image store is the `oci` directory beside
/// the scratch root (not inside it, where the start-up sweep would remove it), the delegated
/// cgroup is unknown until the runtime records it, and a scratch root without a parent has
/// nowhere to put the store. It catches settings that put the store inside the sweep's reach,
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
    let rootless = Settings::for_crun("/usr/bin/crun".into(), "jsjob", (1, 1), Vec::new())
        .placed_beside(Path::new("/"), None);
    assert!(rootless.unwrap_err().contains("has no parent"));
}

/// The command line of a crun agent on the node, less `without`.
fn crun_cli(without: &str) -> Cli {
    let node = node();
    let (state, scratch) = (
        node.state.display().to_string(),
        node.scratch.display().to_string(),
    );
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend(["--runtime", "crun", "--job-user", Fixture::USER]);
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

/// Asserts the whole path with `--runtime crun`: the agent passes its start check on the
/// node's stand-in files (it does not exit 2 saying crun jobs are unavailable), registers, takes
/// a run the server queues, starts it through `crun run` of the stand-in with the run's
/// container name, reports it running while the stand-in holds it, and reports it exited and
/// succeeded once the stand-in ends; the agent then stops with status 0. It catches an agent
/// that checks the host and then runs no crun job, a run that never reaches `crun`, and a
/// result that is lost on the way back.
#[test]
fn an_agent_started_with_runtime_crun_runs_a_job_through_crun() {
    let node = node();
    let server = Server::start_pool("jobs_crun_e2e", &[], POOL_RUNS_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let (crun, state, scratch) = (
        node.crun.display().to_string(),
        node.state.display().to_string(),
        node.scratch.display().to_string(),
    );
    let flags = [
        "--host-id",
        "h1",
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
    let machine = Arc::new(CrunMachine {
        system: System::new().with_crun(seams()),
        fake: FakeMachine::new(),
    });
    let agent = Agent::spawn_machine(
        server.url(),
        server.join_file(),
        &flags,
        host,
        Redirect(address),
        machine,
    );
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
    let beat = || {
        agent.elapse();
        assert_eq!(agent.next_wait(), Duration::from_secs(5));
        assert!(worker.settle());
    };
    let view = |run: &str| {
        let listing = server.get(&format!("/v1/leases/{lease}/runs"));
        let runs = listing["runs"].as_array().unwrap().clone();
        let entry = runs.iter().find(|entry| entry["run_id"] == run);
        let entry = entry.unwrap_or_else(|| panic!("{run} is not listed: {runs:?}"));
        (
            entry["state"].as_str().unwrap().to_owned(),
            entry["outcome"].as_str().map(str::to_owned),
        )
    };

    let queued = server.post_body(
        &format!("/v1/leases/{lease}/runs"),
        &json!({
            "image": format!("registry.example/job:1@{}", digest()),
            "args": ["true"],
            "limits": { "cpu_millis": 4000, "memory_bytes": 1048576 },
        }),
    );
    assert_eq!(queued.status, 202, "{}", queued.body);
    let run = queued.body["run_id"].as_str().unwrap().to_owned();
    beat();
    beat();
    // The stand-in says it is up, and the server lists the run running.
    let dir = node.dir.path();
    let said = fs::read_to_string(dir.join("ready")).expect("read the pipe");
    assert_eq!(said.trim(), "up");
    assert_eq!(view(&run).0, "running");
    let calls = fs::read_to_string(dir.join("calls")).expect("the stand-in's calls");
    let started = calls
        .lines()
        .any(|line| line.contains(" run ") && line.contains(&run));
    assert!(started, "no `crun run` for {run} in\n{calls}");

    fs::write(dir.join("gate"), "go").expect("let the container end");
    // The end is read by the worker's next step: beats are the progress events, and a
    // bound keeps a regression from looping.
    let mut ended = view(&run);
    for _ in 0..100 {
        if ended.1.is_some() {
            break;
        }
        beat();
        ended = view(&run);
    }
    assert_eq!(ended, ("exited".to_owned(), Some("succeeded".to_owned())));
    // While the agent runs, its host id is claimed: a second crun agent with the id is
    // refused, naming the holder.
    let refused = HostClaim::take(node.dir.path(), "h1").expect_err("the id is held");
    assert!(
        refused
            .to_string()
            .contains(&std::process::id().to_string()),
        "{refused}"
    );
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    // The claim goes with the agent: a restart finds it free.
    HostClaim::take(node.dir.path(), "h1").expect("free after the agent ended");
    assert!(!ran.err.contains("does not start jobs"), "{}", ran.err);
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
            .contains(&format!("process {}", std::process::id())),
        "{why}"
    );
    let other = HostClaim::take(dir.path(), "host-b").expect("another id is free");
    drop(first);
    HostClaim::take(dir.path(), "host-a").expect("free once dropped");
    drop(other);
}
