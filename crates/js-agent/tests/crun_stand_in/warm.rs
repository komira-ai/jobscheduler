//! Containers made ahead of their jobs (warm sandboxes) on the crun runtime over the stand-in:
//! the runtime makes them for an image that started recently, a run of that image is started
//! in one (its directories bound in through a fake of the calls that enter a container, its
//! job handed to the stand-in's held init through the lease's pipe), and every other case
//! makes the run cold. The runtime's clock reads 5000 and never moves; the steps of the pool
//! are told the time they run at.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::crun::fs::{FsSettings, Kernel as PathKernel, PathCalls};
use js_agent::crun::{CrunRuntime, CrunSettings};
use js_agent::lifecycle::{MountRoots, RunHandle, RunSpec, Runtime, Status, label};
use js_contain::job_fs::{Attributes, EnterCalls, MountCalls};
use serde_json::Value;

use crate::node::{LONG, NoImages, Node};

/// What a step of the bind is told to do besides noting itself.
type Hook = Box<dyn Fn(&str) -> io::Result<()> + Send + Sync>;

/// The calls that bind a run's directories into a container, recorded as lines; each runs
/// the hook first, which may fail it or change the node.
struct Enter {
    lines: Mutex<Vec<String>>,
    hook: Hook,
}

impl Enter {
    fn new(hook: Hook) -> Arc<Self> {
        Arc::new(Self {
            lines: Mutex::new(Vec::new()),
            hook,
        })
    }

    fn note(&self, line: String) -> io::Result<()> {
        (self.hook)(&line)?;
        self.lines.lock().expect("lines").push(line);
        Ok(())
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("lines").clone()
    }
}

/// A descriptor to stand for anything opened.
fn any_fd() -> io::Result<OwnedFd> {
    std::fs::File::open("/dev/null").map(OwnedFd::from)
}

impl MountCalls for Enter {
    fn clone_tree(&self, _: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        any_fd()
    }
    fn set_attributes(&self, _: BorrowedFd<'_>, _: Attributes) -> io::Result<()> {
        Ok(())
    }
    fn attach(&self, _: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        self.note(format!("attach {}", point.display()))
    }
    fn mount_overlay(&self, _: &Path, _: bool, _: &str) -> io::Result<()> {
        Ok(())
    }
    fn detach(&self, point: &Path) -> io::Result<()> {
        self.note(format!("detach {}", point.display()))
    }
    fn is_mount_point(&self, _: &Path) -> io::Result<bool> {
        Ok(true)
    }
    /// The mounts' directory, made a mount of its own when the runtime opens.
    fn bind_onto_itself(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn make_private(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

impl EnterCalls for Enter {
    fn open_source(&self, path: &Path) -> io::Result<OwnedFd> {
        self.note(format!("source {}", path.display()))?;
        any_fd()
    }
    fn is_dir(&self, _: BorrowedFd<'_>) -> io::Result<bool> {
        Ok(true)
    }
    fn open_namespace(&self, pid: u32) -> io::Result<OwnedFd> {
        self.note(format!("enter {pid}"))?;
        any_fd()
    }
    fn open_root(&self, _: u32) -> io::Result<OwnedFd> {
        any_fd()
    }
    fn unshare_fs(&self) -> io::Result<()> {
        Ok(())
    }
    fn join(&self, _: BorrowedFd<'_>) -> io::Result<()> {
        Ok(())
    }
    fn change_dir(&self, _: BorrowedFd<'_>) -> io::Result<()> {
        Ok(())
    }
    fn change_root(&self) -> io::Result<()> {
        Ok(())
    }
    fn set_fs_ids(&self, _: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
    fn make_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn make_file(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// The reads of host paths: the kernel's lookups and checks, and a search and a hand-over
/// that pass (the node's ids are not the test's).
#[derive(Debug)]
struct Paths;

impl PathCalls for Paths {
    fn open_beneath(
        &self,
        dir: BorrowedFd<'_>,
        rest: &Path,
    ) -> Result<OwnedFd, js_contain::job_fs::ResolveError> {
        PathKernel.open_beneath(dir, rest)
    }
    fn search_as(&self, _: &Path, _: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
    fn answers(&self, dir: &Path) -> io::Result<()> {
        PathKernel.answers(dir)
    }
    fn give_at(&self, _: BorrowedFd<'_>, _: &std::ffi::OsStr, _: (u32, u32)) -> io::Result<()> {
        Ok(())
    }
}

/// The settings of a runtime on `node` that keeps `count` sandboxes an image, its leases
/// beside the runs and its binds made by `enter`.
fn settings(node: &Node, enter: &Arc<Enter>, count: u32) -> CrunSettings {
    let data = node.dir.join("data");
    std::fs::create_dir_all(data.join("sdk/bin")).expect("a mount's source");
    let mut settings = node.settings("/root");
    settings.oci.leases_in = Some(node.dir.join("runs"));
    settings.warm_sandboxes = count;
    settings.fs = Some(FsSettings {
        dir: node.dir.join("mounts"),
        roots: MountRoots::new(&[("data".to_owned(), data)]).expect("roots"),
        calls: enter.clone(),
        paths: Arc::new(Paths),
        inject: enter.clone(),
        image_allow: Vec::new(),
    });
    settings
}

/// A runtime of `settings` whose removals end a container by force at once.
fn open(settings: CrunSettings) -> CrunRuntime {
    let hooks = Node::hooks(Duration::ZERO, LONG);
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// The lease name of the `n`th sandbox of a runtime opened at 5000.
fn warm_name(n: u32) -> String {
    format!("js-{}-w5000-{n}", tag())
}

/// The agent tag of `node-1`, as the runtime names its leases: FNV-1a of the id.
fn tag() -> String {
    let hash = "node-1"
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// The lease name of a cold run `run_id`.
fn cold_name(run_id: &str) -> String {
    format!("js-{}-{run_id}", tag())
}

/// Makes the lease directory the lifecycle makes for `spec` beside its scratch directory.
fn lifecycle_lease(runtime: &CrunRuntime, spec: &RunSpec) -> PathBuf {
    let dir = runtime
        .lease_dir(&spec.run_id)
        .expect("leases beside the runs");
    std::fs::create_dir_all(&dir).expect("the run's lease directory");
    dir
}

/// Starts `spec` cold or warm, as the runtime decides, its lease directory made first.
fn start(runtime: &mut CrunRuntime, spec: &RunSpec) -> RunHandle {
    lifecycle_lease(runtime, spec);
    runtime.create_and_start(spec).expect("started")
}

/// A run of the node's image whose job argument is `part` (the stand-in's held init plays it).
fn job(node: &Node, run_id: &str, part: &str) -> RunSpec {
    let mut spec = node.job(run_id, 60_000);
    spec.body = js_agent::lifecycle::Body::Container {
        image: node.image(),
        args: vec![part.to_owned()],
    };
    spec
}

/// The sidecar of the lease `name`.
fn sidecar(node: &Node, name: &str) -> Value {
    let text = std::fs::read_to_string(node.sidecar(name)).expect("a sidecar");
    serde_json::from_str(&text).expect("json")
}

/// Makes a sandbox for `node`'s image on `runtime`: a cold start of it first (which counts),
/// then a step of the pool, and waits until the sandbox `n` is up.
fn one_sandbox(node: &Node, runtime: &mut CrunRuntime, n: u32) {
    let first = job(node, &format!("r{n}-cold"), "--go");
    let cold = start(runtime, &first);
    assert_eq!(
        cold.0,
        cold_name(&first.run_id),
        "no sandbox yet: the run is cold"
    );
    assert!(!runtime.started_warm(&cold));
    node.pipe(&warm_name(n));
    runtime.tend(5000);
    node.up(&RunHandle(warm_name(n)));
}

/// Proves a run of an image that started recently is started in the sandbox made for it:
/// the sandbox is made at the next step after the image's first start, it is idle (listed
/// as such, and never as a run) until then, and the run's handle is its name; the run's lease
/// directory takes `/work` and the home, its scratch directory is bound after them, into the
/// container's init's process; the job handed to the init is the image's program with the
/// run's arguments, in `/work`, with the image's environment and the two defaults; the lease
/// is given the run's limits; the sidecar records the run, its booking and the sandbox's id;
/// the run's end is read as any run's, and its removal leaves nothing of the sandbox. Catches
/// a run started in a sandbox that runs the wrong program, binds its directories elsewhere,
/// keeps the sandbox's small limits, or cannot be adopted after a restart.
#[test]
fn a_run_starts_in_the_sandbox_made_for_its_image() {
    let node = Node::new("warm-starts");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 2));
    assert!(runtime.idle_handles().is_empty());
    one_sandbox(&node, &mut runtime, 1);
    assert_eq!(runtime.idle_handles(), [RunHandle(warm_name(1))]);
    let listed = runtime.list_labelled().expect("listed");
    assert!(listed.iter().all(|found| found.id.0 != warm_name(1)));
    let idle = sidecar(&node, &warm_name(1));
    assert_eq!(idle["state"], "starting");
    assert_eq!(idle["labels"][label::WARM], node.image());

    let spec = job(&node, "r2", "--exit3");
    let lease = lifecycle_lease(&runtime, &spec);
    let id = runtime.create_and_start(&spec).expect("started");
    assert_eq!(id, RunHandle(warm_name(1)));
    assert!(runtime.started_warm(&id));
    assert!(runtime.idle_handles().is_empty());
    let pid = std::fs::read_to_string(node.cgroup_of(&id.0).join("ctr/cgroup.procs"))
        .expect("the init's group");
    assert_eq!(
        enter.lines(),
        [
            format!("source {}", lease.join("upper").display()),
            format!("source {}", lease.join("rw/home").display()),
            format!("source {}", spec.scratch.display()),
            format!("enter {}", pid.trim()),
            "attach /work".to_owned(),
            "attach /root".to_owned(),
            "attach /scratch".to_owned(),
        ]
    );
    for dir in ["root", "upper", "work", "rw/home"] {
        assert!(lease.join(dir).is_dir(), "{dir}");
    }
    let cgroup = node.cgroup_of(&id.0);
    let read = |file: &str| std::fs::read_to_string(cgroup.join(file)).expect(file);
    assert_eq!(read("memory.max"), (1u64 << 30).to_string());
    assert_eq!(read("memory.swap.max"), "0");
    assert_eq!(read("cpu.max"), "150000 100000");
    assert_eq!(read("ctr/pids.max"), "4096");
    runtime.wait_ended(&id);
    assert!(matches!(runtime.poll(&id), Ok(Status::Exited(exit)) if exit.code == 3));
    let handed = std::fs::read_to_string(node.dir.join(format!("job-{}", id.0))).expect("job");
    assert_eq!(
        handed.lines().collect::<Vec<_>>(),
        [
            "js-job/1",
            "/work",
            "2",
            "/bin/tool",
            "--exit3",
            "4",
            "A=1",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "HOSTNAME=localhost",
            "HOME=/root"
        ]
    );
    let bound = sidecar(&node, &id.0);
    assert_eq!(bound["run_id"], "r2");
    assert_eq!(bound["lease_id"], "w5000-1");
    assert_eq!(bound["state"], "running");
    assert_eq!(bound["labels"][label::RUN], "r2");
    assert!(bound["labels"].get(label::WARM).is_none());
    assert_eq!(bound["booked"]["memory_bytes"], 1u64 << 30);
    runtime.remove(&id).expect("removed");
    assert!(!runtime.started_warm(&id));
    assert!(!node.sidecar(&id.0).exists());
    assert!(!cgroup.exists());
}

/// Proves a run the pool cannot carry is made cold even with a sandbox of its image up, and
/// the sandbox stays for the next: a run with a process limit of its own is carried (the limit
/// is written), one asking for a network is not. Catches a sandbox given to a run whose
/// container differs from it in what a bind cannot change.
#[test]
fn a_run_a_sandbox_cannot_carry_is_made_cold() {
    let node = Node::new("warm-unfit");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let mut wired = job(&node, "r2", "--go");
    wired.network = js_types::JobNetwork::AllowList;
    lifecycle_lease(&runtime, &wired);
    let why = runtime.create_and_start(&wired).expect_err("no egress");
    assert_eq!(
        why.to_string(),
        "the job asked for allow-list and this agent's ceiling is none"
    );
    assert_eq!(runtime.idle_handles(), [RunHandle(warm_name(1))]);
    let mut limited = job(&node, "r3", "--go");
    limited.pids = Some(77);
    let id = start(&mut runtime, &limited);
    assert_eq!(id.0, warm_name(1));
    let pids = std::fs::read_to_string(node.cgroup_of(&id.0).join("ctr/pids.max"));
    assert_eq!(pids.expect("pids.max"), "77");
    runtime.remove(&id).expect("removed");
}

/// Proves a sandbox whose bind fails is removed and the run is made cold in its own lease, its
/// lease directory emptied of what the bind made, as is one whose job cannot be handed (its
/// pipe gone): no sandbox is reused after a failure, and the run starts either way. Catches a
/// failed bind that fails the run, or leaves the run's lease directory unfit for its cold make.
#[test]
fn a_failed_bind_makes_the_run_cold_and_removes_the_sandbox() {
    for (n, failing) in [(0, "attach /scratch"), (1, "attach /root")] {
        let node = Node::new(&format!("warm-bind-fails-{n}"));
        let pipe_of = node
            .dir
            .join("runs")
            .join(format!(".lease-{}", warm_name(1)));
        let hook: Hook = match n {
            0 => Box::new(move |line| match line == failing {
                true => Err(io::Error::other("refused")),
                false => Ok(()),
            }),
            _ => Box::new(move |line| {
                if line == failing {
                    std::fs::remove_file(pipe_of.join("job"))?;
                }
                Ok(())
            }),
        };
        let enter = Enter::new(hook);
        let mut runtime = open(settings(&node, &enter, 1));
        one_sandbox(&node, &mut runtime, 1);
        let spec = job(&node, "r2", "--go");
        let id = start(&mut runtime, &spec);
        assert_eq!(id.0, cold_name("r2"), "case {n}");
        assert!(!runtime.started_warm(&id));
        assert!(runtime.idle_handles().is_empty());
        assert!(
            !node.sidecar(&warm_name(1)).exists(),
            "case {n}: the sandbox is gone"
        );
        runtime.remove(&id).expect("removed");
    }
}

/// Proves the pool's steps: a sandbox that ended while idle is removed and replaced; an image
/// no run started for [`KEEP`](super) loses its sandboxes; an image whose sandbox cannot be
/// made (an image the store cannot fetch) rests and is not tried again at the next step.
/// Catches a pool that hands out a dead sandbox, keeps sandboxes for an image nobody runs, or
/// tries a failing make at every step.
#[test]
fn the_pool_replaces_what_ended_drops_what_is_unused_and_rests_a_failing_image() {
    let node = Node::new("warm-tend");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    // An empty job: the held init ends.
    std::fs::OpenOptions::new()
        .write(true)
        .open(
            node.dir
                .join("runs")
                .join(format!(".lease-{}/job", warm_name(1))),
        )
        .expect("the pipe");
    runtime.wait_ended(&RunHandle(warm_name(1)));
    node.pipe(&warm_name(2));
    runtime.tend(5000);
    node.up(&RunHandle(warm_name(2)));
    assert_eq!(runtime.idle_handles(), [RunHandle(warm_name(2))]);
    runtime.tend(5000 + 10 * 60 * 1000);
    assert!(runtime.idle_handles().is_empty());
    assert!(!node.sidecar(&warm_name(2)).exists());

    drop(runtime);
    let node = Node::new("warm-rests");
    let mut runtime = open(settings(&node, &enter, 1));
    std::fs::create_dir_all(node.dir.join("runs")).expect("runs");
    let mut other = job(&node, "r3", "--go");
    other.body = js_agent::lifecycle::Body::Container {
        image: format!("registry.example/other@sha256:{}", "cd".repeat(32)),
        args: Vec::new(),
    };
    lifecycle_lease(&runtime, &other);
    runtime.create_and_start(&other).expect_err("no such image");
    runtime.tend(5000);
    assert!(runtime.idle_handles().is_empty());
    let made = std::fs::read_dir(node.scratch.join("oci-labels"))
        .expect("ledger")
        .count();
    runtime.tend(5001);
    let again = std::fs::read_dir(node.scratch.join("oci-labels"))
        .expect("ledger")
        .count();
    assert_eq!(made, again, "a resting image is not tried again");
}

/// Proves a restart removes an idle sandbox (its sidecar never ran) and adopts a run started
/// in one, keeping the run's own lease directory, which no sidecar is named for. Catches a
/// restart that adopts an idle sandbox as a run, or removes a live run's writable layers.
#[test]
fn a_restart_removes_idle_sandboxes_and_adopts_the_runs_started_in_one() {
    let node = Node::new("warm-restart");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 2));
    one_sandbox(&node, &mut runtime, 1);
    let spec = job(&node, "r2", "--hold");
    let lease = lifecycle_lease(&runtime, &spec);
    let id = runtime.create_and_start(&spec).expect("started");
    assert_eq!(id.0, warm_name(1));
    node.pipe(&warm_name(2));
    runtime.tend(5000);
    node.up(&RunHandle(warm_name(2)));
    // The process `crun state` names, in the run's container's group, as a restart checks.
    node.place(&id.0, crate::node::PID);
    drop(runtime);

    let mut again = open(settings(&node, &enter, 2));
    let swept = again.swept().clone();
    assert!(swept.adopted.contains(&"r2".to_owned()), "{swept:?}");
    assert!(
        swept.removed.iter().any(|r| r.name == warm_name(2)),
        "{swept:?}"
    );
    assert!(
        lease.join("upper").is_dir(),
        "the adopted run's lease directory stays"
    );
    let listed = again.list_labelled().expect("listed");
    let run = listed.iter().find(|found| found.id == id).expect("the run");
    assert_eq!(run.labels[label::RUN], "r2");
    assert!(listed.iter().all(|found| !found.id.0.contains("-w5000-2")));
    let why = again.remove_orphan("r2").expect_err("held");
    assert_eq!(why.to_string(), "run r2 is not an orphan");
    again.remove(&id).expect("removed");
}

/// Proves a runtime that keeps no sandboxes makes none at a step, and starts every run cold.
/// Catches a pool that runs with `--warm-sandboxes 0`.
#[test]
fn a_runtime_that_keeps_no_sandboxes_makes_none() {
    let node = Node::new("warm-off");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 0));
    let spec = job(&node, "r1", "--go");
    let id = start(&mut runtime, &spec);
    runtime.tend(5000);
    assert!(runtime.idle_handles().is_empty());
    assert_eq!(id.0, cold_name("r1"));
    assert!(enter.lines().is_empty());
}

/// Proves a sandbox that cannot be made leaves nothing behind and rests its image: its `crun`
/// cannot be started, its bundle cannot be written, or its pipe cannot be given to the job.
/// Catches a failed make that leaves a lease, or one tried at every step.
#[test]
fn a_sandbox_that_cannot_be_made_leaves_nothing_and_rests_its_image() {
    for name in ["warm-start-fails", "warm-no-bundle", "warm-pipe-denied"] {
        let node = Node::new(name);
        let enter = Enter::new(Box::new(|_| Ok(())));
        let mut runtime = open(settings(&node, &enter, 1));
        let spec = job(&node, "r1", "--go");
        lifecycle_lease(&runtime, &spec);
        let _ = runtime.create_and_start(&spec);
        runtime.tend(5000);
        assert!(runtime.idle_handles().is_empty(), "{name}");
        assert!(!node.sidecar(&warm_name(1)).exists(), "{name}");
        runtime.tend(5001);
        assert!(!node.sidecar(&warm_name(2)).exists(), "{name}: rests");
    }
}

/// Proves a sandbox that cannot be removed is let go: it is no longer idle, never a run, and
/// its sidecar stays for the next start of the agent. Catches a pool that keeps handing out a
/// sandbox it failed to remove, or lists it as a run.
#[test]
fn a_sandbox_that_cannot_be_removed_is_left_to_the_next_start() {
    let node = Node::new("warm-stays");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    std::fs::write(node.dir.join("delete-fails"), b"").expect("a failing delete");
    runtime.tend(5000 + 10 * 60 * 1000);
    assert!(runtime.idle_handles().is_empty());
    assert!(node.sidecar(&warm_name(1)).exists());
    let listed = runtime.list_labelled().expect("listed");
    assert!(listed.iter().all(|found| found.id.0 != warm_name(1)));
    std::fs::remove_file(node.dir.join("delete-fails")).expect("delete works");
}

/// Proves a run whose sidecar cannot be written once it started in a sandbox fails its start,
/// and the run is removed (here its removal fails too, as the ledger cannot be written, and is
/// left to the next start). Catches a run that runs with a sidecar a restart would remove as a
/// sandbox that never ran.
#[test]
fn a_run_whose_sidecar_cannot_be_written_fails_its_start() {
    let node = Node::new("warm-no-sidecar");
    let ledger = node.scratch.join("oci-labels");
    let locked = ledger.clone();
    let hook: Hook = Box::new(move |line| {
        if line == "attach /scratch" {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::Permissions::from_mode(0o500);
            std::fs::set_permissions(&locked, mode)?;
        }
        Ok(())
    });
    let enter = Enter::new(hook);
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let spec = job(&node, "r2", "--hold");
    lifecycle_lease(&runtime, &spec);
    let why = runtime.create_and_start(&spec).expect_err("no sidecar");
    assert!(
        why.to_string().starts_with("record the running lease: "),
        "{why}"
    );
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o700)).expect("mode");
    // Its removal failed with the ledger: the run is still held, and goes at the next try.
    let listed = runtime.list_labelled().expect("listed");
    let run = listed.iter().find(|found| found.labels[label::RUN] == "r2");
    assert_eq!(run.map(|found| found.id.0.clone()), Some(warm_name(1)));
    runtime.remove(&RunHandle(warm_name(1))).expect("removed");
}

/// Proves the run's files are given to the job and bound read-only at `/run/js-files` after
/// its scratch directory; a run naming files on a runtime with no files root, or whose
/// image's config is gone, is not started in the sandbox (which is removed) and is made cold,
/// where it fails as a cold start does. Catches a warm run without its files, or one handed a
/// program its image's config does not give.
#[test]
fn a_runs_files_are_bound_last_and_what_a_bind_cannot_give_fails_cold() {
    let node = Node::new("warm-files");
    let (files_dir, root) = node.files_root();
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut with_files = settings(&node, &enter, 1);
    with_files.files = Some(root.clone());
    let mut runtime = open(with_files);
    one_sandbox(&node, &mut runtime, 1);
    let file = js_agent_wire::RunFile {
        name: "token".to_owned(),
        contents: "secret".to_owned(),
    };
    let dir = root.write("r2", &[file]).expect("files");
    let mut spec = job(&node, "r2", "--go");
    spec.files = Some(dir.clone());
    let id = start(&mut runtime, &spec);
    assert_eq!(id.0, warm_name(1));
    let lines = enter.lines();
    assert_eq!(
        lines.last().map(String::as_str),
        Some("attach /run/js-files")
    );
    assert!(lines.contains(&format!("source {}", dir.display())));
    assert!(
        crate::node::given(&files_dir)
            .iter()
            .any(|(path, _, _)| *path == dir)
    );
    runtime.remove(&id).expect("removed");
    drop(runtime);

    let node = Node::new("warm-no-files");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let mut spec = job(&node, "r2", "--go");
    spec.files = Some(node.dir.join("files-of-r2"));
    lifecycle_lease(&runtime, &spec);
    let why = runtime.create_and_start(&spec).expect_err("no files root");
    assert_eq!(why.to_string(), "this runtime has no run files directory");
    assert!(runtime.idle_handles().is_empty());

    one_sandbox(&node, &mut runtime, 2);
    let hex = &node.digest[7..];
    let config = js_agent::crun::config_dir(&node.scratch).join(format!("{hex}.json"));
    std::fs::remove_file(config).expect("the config goes");
    let spec = job(&node, "r3", "--go");
    lifecycle_lease(&runtime, &spec);
    runtime
        .create_and_start(&spec)
        .expect_err("no config, no image to fetch it from");
    assert!(runtime.idle_handles().is_empty());
}

/// Proves a run whose lease directory the lifecycle did not make is not started in a sandbox:
/// the sandbox is removed and the cold make refuses the run as it would without one. Catches
/// a warm run whose writes would escape the run's quota.
#[test]
fn a_run_without_its_lease_directory_is_refused_warm_and_cold() {
    let node = Node::new("warm-no-lease");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let spec = job(&node, "r2", "--go");
    runtime
        .create_and_start(&spec)
        .expect_err("no lease directory");
    assert!(runtime.idle_handles().is_empty());
    assert!(!node.sidecar(&warm_name(1)).exists());
}

/// Asks `what` until it is not pending: the lookups and the assembly run on the runtime's
/// threads, which the test only waits for (as the layered tests do).
fn settled(
    mut what: impl FnMut() -> js_agent::lifecycle::FsPrepared,
) -> js_agent::lifecycle::FsPrepared {
    for _ in 0..30_000 {
        match what() {
            js_agent::lifecycle::FsPrepared::Pending => {
                std::thread::sleep(Duration::from_millis(1))
            }
            other => return other,
        }
    }
    panic!("the runtime's threads did not answer");
}

/// Proves a run with a mount of a host path is started in a sandbox: the clone its assembly
/// made is bound at the mount's target after the scratch directory, and the run's removal and
/// the lifecycle's discard unmount it by the run's own lease name. Catches a warm run that
/// loses its mounts, or whose mounts are looked for under the sandbox's name.
#[test]
fn a_run_with_a_mount_starts_in_a_sandbox_with_its_clone_bound() {
    use js_agent::lifecycle::{FsPrepared, JobFs};
    use js_agent_wire::{JobMount, MountMode, MountSource, Propagation};
    let node = Node::new("warm-mount");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let spec = job(&node, "r2", "--exit3");
    let fs = JobFs {
        layers: Vec::new(),
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: node.dir.join("data/sdk").display().to_string(),
            },
            target: "/opt/sdk".to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: false,
            propagation: Propagation::Private,
        }],
    };
    let image = node.image();
    assert_eq!(
        runtime.resolve_fs("r2", &image, &fs, 5000),
        FsPrepared::Pending
    );
    assert_eq!(
        settled(|| runtime.resolve_fs("r2", &image, &fs, 5000)),
        FsPrepared::Ready
    );
    lifecycle_lease(&runtime, &spec);
    assert_eq!(
        settled(|| runtime.assemble_fs("r2", 5000)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    assert_eq!(id.0, warm_name(1));
    let attached: Vec<String> = enter
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("attach /") && !line.contains("/mounts/"))
        .collect();
    assert!(
        attached.ends_with(&["attach /scratch".to_owned(), "attach /opt/sdk".to_owned()]),
        "{attached:?}"
    );
    runtime.wait_ended(&id);
    // A run that did not succeed is held until its sources answered, looked up by its own
    // lease's name.
    let status = settled_status(&mut runtime, &id);
    assert!(
        matches!(status, Status::Exited(exit) if exit.code == 3),
        "{status:?}"
    );
    assert_eq!(runtime.input_failure(&id), None);
    runtime.remove(&id).expect("removed");
    runtime.discard_fs("r2").expect("discarded");
}

/// Polls the run `id` until its end is read and its sources answered.
fn settled_status(runtime: &mut CrunRuntime, id: &RunHandle) -> Status {
    for _ in 0..30_000 {
        match runtime.poll(id).expect("polled") {
            Status::Running => std::thread::sleep(Duration::from_millis(1)),
            other => return other,
        }
    }
    panic!("the run's sources did not answer");
}

/// Proves a restart that adopts a run started in a sandbox keeps the mounts its assembly made
/// under the run's own lease name (nothing of them is detached), and the adopted run's sources
/// are found there. Catches a restart that unmounts a running warm job's inputs because no
/// container bears their name.
#[test]
fn a_restart_keeps_the_mounts_of_a_run_started_in_a_sandbox() {
    use js_agent::lifecycle::{FsPrepared, JobFs};
    use js_agent_wire::{JobMount, MountMode, MountSource, Propagation};
    let node = Node::new("warm-restart-mount");
    let enter = Enter::new(Box::new(|_| Ok(())));
    let mut runtime = open(settings(&node, &enter, 1));
    one_sandbox(&node, &mut runtime, 1);
    let spec = job(&node, "r2", "--hold");
    let fs = JobFs {
        layers: Vec::new(),
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: node.dir.join("data/sdk").display().to_string(),
            },
            target: "/opt/sdk".to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: false,
            propagation: Propagation::Private,
        }],
    };
    let image = node.image();
    assert_eq!(
        runtime.resolve_fs("r2", &image, &fs, 5000),
        FsPrepared::Pending
    );
    assert_eq!(
        settled(|| runtime.resolve_fs("r2", &image, &fs, 5000)),
        FsPrepared::Ready
    );
    lifecycle_lease(&runtime, &spec);
    assert_eq!(
        settled(|| runtime.assemble_fs("r2", 5000)),
        FsPrepared::Ready
    );
    let id = runtime.create_and_start(&spec).expect("started");
    assert_eq!(id.0, warm_name(1));
    node.place(&id.0, crate::node::PID);
    let mounts = runtime.mounts_of("r2").expect("the run's mounts");
    drop(runtime);

    let before = enter.lines().len();
    let mut again = open(settings(&node, &enter, 1));
    assert!(again.swept().adopted.contains(&"r2".to_owned()));
    let detached: Vec<String> = enter.lines()[before..]
        .iter()
        .filter(|line| line.starts_with("detach") && line.contains(&cold_name("r2")))
        .cloned()
        .collect();
    assert!(detached.is_empty(), "{detached:?}");
    assert!(mounts.is_dir(), "the run's mounts stay");
    again.remove(&id).expect("removed");
    again.discard_fs("r2").expect("discarded");
}
