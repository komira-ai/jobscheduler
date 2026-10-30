//! A node of directories and a script for `crun`.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use js_agent::crun::{CgroupSettings, CrunRuntime, CrunSettings, Hooks, config_dir};
use js_agent::lifecycle::{Body, DirLayout, FilesRoot, RunHandle, RunSpec, label};
use js_contain::OciConfig;
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::subids::IdFiles;
use js_testkit::dir::TestDir;

/// The first host uid of the subordinate range of the node's job user.
pub const SUB_UID: u32 = 100_000;
/// The first host gid of the subordinate range.
pub const SUB_GID: u32 = 200_000;

/// Every path given away, with the ids.
pub type Given = Vec<(PathBuf, u32, u32)>;

static GIVEN: Mutex<Given> = Mutex::new(Vec::new());

/// Gives nothing away: notes it, and refuses a path with `refuse` in it. For the `work`
/// directory of a lease in a node named `start-fails` it also puts a directory where the
/// runtime will make its init report, so that `crun` cannot be started.
fn note(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let text = path.to_string_lossy();
    if text.contains("refuse") {
        return Err(io::Error::other("refused"));
    }
    if text.contains("unwritable-cgroup") && path.ends_with("cgroup.threads") {
        return Err(io::Error::other("refused"));
    }
    if text.contains("no-bundle") && path.ends_with("work") {
        std::fs::File::create(path.parent().expect("a lease").join("bundle"))?;
    }
    if text.contains("start-fails") && path.ends_with("work") {
        let report = path.parent().expect("a lease").join("init-report");
        std::fs::create_dir(report)?;
    }
    GIVEN
        .lock()
        .expect("given")
        .push((path.to_owned(), uid, gid));
    Ok(())
}

fn clock() -> u64 {
    5_000
}

/// The paths given away below `dir`, in order.
pub fn given(dir: &Path) -> Given {
    let all = GIVEN.lock().expect("given").clone();
    all.into_iter()
        .filter(|(path, _, _)| path.starts_with(dir))
        .collect()
}

/// An image source that fetches nothing: the image of every test is in the store, and one
/// that is not fails to be fetched.
#[derive(Debug)]
pub struct NoImages;

#[async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed("no registry in this test".to_owned()))
    }
}

/// An image source whose fetch never ends.
#[derive(Debug)]
pub struct Endless;

#[async_trait]
impl ImageSource for Endless {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        std::future::pending().await
    }
}

/// The stand-in. Its behaviour is chosen by the end of the container's name (a run id such
/// as `r1-hold`). `delete` makes the lease cgroup's files go, as removing a cgroup does,
/// and fails when the node has a file `delete-fails`.
const CRUN: &str = r#"#!/bin/sh
MOUNT=@MOUNT@
DIR=@DIR@
root=; log=; bundle=; cmd=; rest=
while [ $# -gt 0 ]; do
  case "$1" in
    --root) root=$2; shift 2;;
    --log) log=$2; shift 2;;
    --log-format|--pid-file|--console-socket|--signal) shift 2;;
    --bundle|-b) bundle=$2; shift 2;;
    -*) shift;;
    *) if [ -z "$cmd" ]; then cmd=$1; else rest="$rest $1"; fi; shift;;
  esac
done
set -- $rest
name=$1
cg="$MOUNT/actions/$name"
case "$cmd" in
  run)
    mkdir -p "$root/$name"
    echo $$ > "$root/pid"
    case "$name" in
      *-exit) exit 3;;
      *-say) echo to-stdout; echo to-stderr >&2; exit 0;;
      *-fail) echo '{"level":"error","msg":"cgroup: no"}' >> "$log"; exit 1;;
      *-oom) printf 'low 0\nhigh 0\nmax 1\noom 1\noom_kill 1\n' > "$cg/memory.events"; exit 137;;
      *-busy) printf 'low 0\nhigh 0\nmax 0\noom 0\noom_kill 1\n' > "$cg/memory.events"; exit 137;;
      *-hold)
        mkfifo "$root/gate"
        cat "$root/gate" > /dev/null 2>&1 &
        c=$!
        trap 'kill $c; exit 143' TERM
        echo up > "$DIR/ready-$name"
        wait $c;;
      *-stubborn)
        trap '' TERM
        mkfifo "$cg/cgroup.kill"
        echo up > "$DIR/ready-$name"
        read x < "$cg/cgroup.kill"
        exit 137;;
      *-stuck)
        trap '' TERM
        mkfifo "$root/gate"
        echo up > "$DIR/ready-$name"
        read x < "$root/gate"
        exit 137;;
      *) exit 0;;
    esac;;
  kill)
    kill -TERM "$(cat "$root/pid")";;
  delete)
    if [ -e "$DIR/delete-fails" ]; then echo boom >&2; exit 1; fi
    find "$cg" -maxdepth 1 ! -type d -delete 2>/dev/null
    rmdir "$root/$name" 2>/dev/null
    exit 0;;
  state) echo '{"pid":1}';;
esac
"#;

/// A node: ids, id tools, the stand-in, cgroup directories and an image in the store.
pub struct Node {
    pub dir: TestDir,
    pub scratch: PathBuf,
    pub cgroups: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub digest: String,
}

impl Node {
    /// A node for the test `name` (a name with `refuse` in it has paths that cannot be given
    /// away; one with `start-fails` has runs whose `crun` cannot be started).
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let ids = IdFiles::in_dir(&dir.join("ids"));
        std::fs::create_dir_all(dir.join("ids")).expect("ids");
        std::fs::write(&ids.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
        std::fs::write(&ids.subuid, format!("{uid}:{SUB_UID}:65536\n")).expect("subuid");
        std::fs::write(&ids.subgid, format!("{uid}:{SUB_GID}:65536\n")).expect("subgid");
        let tools = dir.join("tools");
        std::fs::create_dir_all(&tools).expect("tools");
        for tool in ["newuidmap", "newgidmap"] {
            let path = tools.join(tool);
            std::fs::write(&path, b"#!/bin/true\n").expect("tool");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755))
                .expect("chmod");
        }
        let cgroups = dir.join("cgroup");
        for part in ["root", "refuse-root", "actions"] {
            std::fs::create_dir_all(cgroups.join(part)).expect("cgroup");
        }
        // The files the runtime gives to the job user exist in a cgroup directory.
        let files = [
            ("root", "cgroup.procs"),
            ("actions", "cgroup.procs"),
            ("actions", "cgroup.threads"),
            ("actions", "cgroup.subtree_control"),
        ];
        for (part, file) in files {
            std::fs::write(cgroups.join(part).join(file), b"").expect("a cgroup file");
        }
        let script = CRUN
            .replace("@MOUNT@", &cgroups.display().to_string())
            .replace("@DIR@", &dir.display().to_string());
        let crun = dir.join("crun");
        std::fs::write(&crun, script).expect("crun");
        std::fs::set_permissions(&crun, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let digest = format!("sha256:{}", "ab".repeat(32));
        let scratch = dir.join("oci");
        let hex = &digest[7..];
        std::fs::create_dir_all(scratch.join("oci-images").join(hex).join("rootfs"))
            .expect("image");
        std::fs::create_dir_all(config_dir(&scratch)).expect("configs");
        let config = format!(
            r#"{{"config":{{"Entrypoint":["/bin/tool"],"Env":["A=1"]}},"rootfs":{{"diff_ids":["sha256:{}"]}}}}"#,
            "0".repeat(64)
        );
        std::fs::write(config_dir(&scratch).join(format!("{hex}.json")), config).expect("config");
        Self {
            dir,
            scratch,
            cgroups,
            uid,
            gid,
            digest,
        }
    }

    /// The settings of a runtime on this node, with the agent's cgroup `root`.
    pub fn settings(&self, root: &str) -> CrunSettings {
        CrunSettings {
            oci: OciConfig {
                runtime: self.dir.join("crun"),
                daemon: std::env::current_exe().expect("test binary"),
                id_files: IdFiles::in_dir(&self.dir.join("ids")),
                check_search: false,
                id_tool_path: Some(self.dir.join("tools").into_os_string()),
                job_user: Some((self.uid, self.gid)),
                ..OciConfig::default()
            },
            scratch: self.scratch.clone(),
            cgroups: CgroupSettings {
                mount: self.cgroups.clone(),
                root: root.to_owned(),
                actions: "/actions".to_owned(),
            },
            host_id: "node-1".to_owned(),
            pull_threads: 1,
            files: None,
        }
    }

    /// The hooks: nothing is given away, the clock reads 5000, the waits are as given.
    pub fn hooks(kill_wait: Duration, pull_limit: Duration) -> Hooks {
        Hooks {
            chown: note,
            now_ms: clock,
            kill_wait,
            pull_limit,
        }
    }

    /// A runtime on this node whose waits are long.
    pub fn open(&self) -> CrunRuntime {
        self.open_with(Self::hooks(LONG, LONG), Arc::new(NoImages))
    }

    /// A runtime whose wait after a kill is spent at once.
    pub fn open_impatient(&self) -> CrunRuntime {
        self.open_with(Self::hooks(Duration::ZERO, LONG), Arc::new(NoImages))
    }

    /// A runtime with the hooks and image source given.
    pub fn open_with(&self, hooks: Hooks, source: Arc<dyn ImageSource>) -> CrunRuntime {
        CrunRuntime::open_with(self.settings("/root"), source, hooks).expect("opens")
    }

    /// The image of every run, with a tag.
    pub fn image(&self) -> String {
        format!("registry.example/job:1@{}", self.digest)
    }

    /// A run `run_id` of the image, with the argument `--go`.
    pub fn spec(&self, run_id: &str) -> RunSpec {
        let scratch = self.dir.join("runs").join(run_id);
        std::fs::create_dir_all(&scratch).expect("scratch");
        RunSpec {
            run_id: run_id.to_owned(),
            body: Body::Container {
                image: self.image(),
                args: vec!["--go".to_owned()],
            },
            cpu_millis: 1500,
            memory_bytes: 1 << 30,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            scratch,
            files: None,
            slot: 0,
            isolation: js_agent_wire::Isolation::Contained,
            labels: BTreeMap::from([(label::RUN.to_owned(), run_id.to_owned())]),
        }
    }

    /// Makes the pipe the script of the container `name` opens when it is up. Make it
    /// before the run is created.
    pub fn pipe(&self, name: &str) {
        let pipe = self.dir.join(format!("ready-{name}"));
        let made = Command::new("mkfifo").arg(&pipe).status().expect("mkfifo");
        assert!(made.success());
    }

    /// Waits for the script of the container `id` to say it is up.
    pub fn up(&self, id: &RunHandle) {
        let said = std::fs::read_to_string(self.dir.join(format!("ready-{}", id.0)));
        assert_eq!(said.expect("read the pipe").trim(), "up");
    }

    /// The cgroup directory of the lease of the container `name`.
    pub fn cgroup_of(&self, name: &str) -> PathBuf {
        self.cgroups.join("actions").join(name)
    }

    /// The directory of the lease of the container `name`.
    pub fn lease_of(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// A run files root of the layout the job user needs, in a directory of the system's
    /// temporary directory (every directory above it can be searched by others, which the
    /// checkout's may not be). The directory goes with the first value.
    pub fn files_root(&self) -> (TestDir, FilesRoot) {
        let dir = TestDir::under(std::env::temp_dir(), "js-stand-in-files");
        let root = dir.join("files");
        std::fs::create_dir_all(&root).expect("files");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o711)).expect("mode");
        let files = FilesRoot::open_with(&root, DirLayout::Searchable).expect("a files root");
        (dir, files)
    }
}

/// A wait no test comes to the end of.
pub const LONG: Duration = Duration::from_secs(3600);
