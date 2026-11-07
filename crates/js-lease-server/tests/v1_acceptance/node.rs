//! The host the agent runs its jobs on: a tree of directories that passes the `--runtime
//! crun` start check, an image store that holds the one image every job names, cgroup
//! directories, and the stand-in `crun`.
//!
//! The stand-in is a shell script. Every call is appended to `calls`. `run` reads the job's
//! arguments from the bundle's `config.json`, where they are words of the form `tag=NAME`,
//! `exit=CODE`, `say=TEXT` and `hold=yes`: it appends `start NAME` to `events`, prints
//! `TEXT` on its standard output and `TEXT on stderr` on its standard error, and, with
//! `hold=yes`, writes `up` to `up-NAME` and holds until the test makes `go-NAME`. It then
//! appends `end NAME` and exits with `CODE`. A TERM (`crun kill`) appends `stop NAME` and
//! exits 143. A held job whose node was removed (its case ended, passed or failed) ends as
//! well, so no stand-in outlives its case. `up-NAME` is a FIFO the case made ([`Node::expect_up`]): the case blocks on
//! reading it, so it learns the job is up, its TERM handler in place, when the stand-in says
//! so and not after some number of heartbeats.
//!
//! A real `crun run` makes the container's cgroup (`ctr` below the lease's) before the job's
//! program starts, and the agent's first look at a running job finds it there; a host whose
//! running job has no cgroup is tainted as one the agent cannot see. The stand-in starts
//! after the container's supervisor has said it is ready, so [`before_supervise`] makes the
//! cgroup in the supervisor itself, before the supervisor runs: the agent never sees a
//! running job without one.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};

use js_agent::crun::{CgroupSettings, CgroupSource, Hooks, IdSources, Seams};
use js_agent::crun_host::CrunHost;
use js_contain::subids::IdFiles;
use js_testkit::dir::TestDir;
use rustix::fs::{CWD, FileType, Mode, mknodat};

/// The job user the agent runs `crun` as.
pub const JOB_USER: &str = "jsjob";

/// The first host uid and gid of the job user's subordinate ranges.
const SUB: u32 = 100_000;

/// The image every job runs: the one image in the node's store, pinned by its digest.
pub fn image() -> String {
    format!("registry.example/job:1@sha256:{}", "ab".repeat(32))
}

/// The stand-in `crun`.
const CRUN: &str = r#"#!/bin/sh
MOUNT=@MOUNT@
DIR=@DIR@
echo "$@" >> "$DIR/calls"
root=; cmd=; rest=; bundle=
while [ $# -gt 0 ]; do
  case "$1" in
    --version) echo "crun version 1.14.1"; echo "commit: abc"; exit 0;;
    --root) root=$2; shift 2;;
    --bundle|-b) bundle=$2; shift 2;;
    --log|--log-format|--pid-file|--console-socket|--signal) shift 2;;
    -*) shift;;
    *) if [ -z "$cmd" ]; then cmd=$1; else rest="$rest $1"; fi; shift;;
  esac
done
set -- $rest
name=$1
cg="$MOUNT/actions/$name"
word() {
  grep -o "\"$1=[^\"]*\"" "$bundle/config.json" | head -n 1 | sed "s/^\"$1=//; s/\"\$//"
}
case "$cmd" in
  run)
    mkdir -p "${root:?}/${name:?}"
    echo $$ > "$root/pid"
    tag=$(word tag); code=$(word exit); say=$(word say); hold=$(word hold)
    trap 'echo "stop $tag" >> "$DIR/events"; exit 143' TERM
    echo "start $tag" >> "$DIR/events"
    echo "$say"
    echo "$say on stderr" >&2
    if [ -n "$hold" ]; then
      echo up > "$DIR/up-$tag"
      while [ ! -e "$DIR/go-$tag" ] && [ -d "$DIR" ]; do sleep 0.02; done
    fi
    echo "end $tag" >> "$DIR/events"
    exit "${code:-0}";;
  kill)
    kill -TERM "$(cat "$root/pid")";;
  delete)
    find "$cg" -maxdepth 1 ! -type d -delete 2>/dev/null
    exit 0;;
  state) echo '{"pid":1}';;
esac
"#;

/// The node of the case that runs now. The crun runtime's seams are function pointers, so
/// what they read is a static: the cases run one at a time and each sets its own node here.
static CURRENT: Mutex<Option<CrunHost>> = Mutex::new(None);

/// The start-check files of the node of the case that runs now.
fn current_host() -> CrunHost {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("a case set its node")
}

/// The host of one case.
pub struct Node {
    /// The directory of the node, removed when the case ends.
    dir: TestDir,
    /// The stand-in `crun`.
    pub crun: PathBuf,
    /// The agent's state root.
    pub state: PathBuf,
    /// The agent's scratch root.
    pub scratch: PathBuf,
    /// The id files the backend reads.
    ids: IdFiles,
    /// Where `newuidmap` and `newgidmap` are.
    tools: PathBuf,
    /// The cgroup tree.
    cgroups: PathBuf,
}

/// Writes `text` to `path`, making its directory.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
    fs::write(path, text).expect("the file");
}

/// Sets the mode of `path`.
fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

impl Node {
    /// A node of its own for the case `name`, made the current one. It is made in the
    /// system's temporary directory, not the target directory: the start check needs every
    /// directory above the scratch root searchable by the job user, and a target directory
    /// below a home directory others cannot search (a CI runner's) would fail it.
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir().join("v1-acceptance"), name);
        let root = dir.path().to_owned();
        set_mode(&root, 0o755);
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let ids = IdFiles::in_dir(&root.join("etc"));
        write(
            &ids.passwd,
            &format!("root:x:0:0::/root:/bin/sh\n{JOB_USER}:x:{uid}:{gid}::/:/usr/sbin/nologin\n"),
        );
        write(&ids.subuid, &format!("{JOB_USER}:{SUB}:65536\n"));
        write(&ids.subgid, &format!("{JOB_USER}:{SUB}:65536\n"));
        let tools = root.join("bin");
        for tool in ["newuidmap", "newgidmap"] {
            write(&tools.join(tool), "#!/bin/true\n");
            set_mode(&tools.join(tool), 0o4755);
        }
        // The start check's view of the agent's own cgroup, and the tree the runtime makes
        // the leases' cgroups in.
        write(
            &root.join("cgroup/system.slice/js-agent.service/cgroup.controllers"),
            "cpuset cpu io memory pids\n",
        );
        write(
            &root.join("self_cgroup"),
            "0::/system.slice/js-agent.service\n",
        );
        write(&root.join("restrict"), "0\n");
        write(&root.join("profiles"), "");
        let cgroups = root.join("runtime-cgroup");
        for (part, file) in [
            ("root", "cgroup.procs"),
            ("actions", "cgroup.procs"),
            ("actions", "cgroup.threads"),
            ("actions", "cgroup.subtree_control"),
        ] {
            write(&cgroups.join(part).join(file), "");
        }
        let crun = tools.join("crun");
        let script = CRUN
            .replace("@MOUNT@", &cgroups.display().to_string())
            .replace("@DIR@", &root.display().to_string());
        write(&crun, &script);
        set_mode(&crun, 0o755);
        let (state, scratch) = (root.join("state"), root.join("scratch"));
        fs::create_dir(&state).expect("the state root");
        set_mode(&state, 0o700);
        fs::create_dir(&scratch).expect("the scratch root");
        set_mode(&scratch, 0o711);
        // The image store is beside the scratch root and holds the one image and its config.
        let store = root.join("oci");
        let hex = "ab".repeat(32);
        fs::create_dir_all(store.join("oci-images").join(&hex).join("rootfs")).expect("image");
        let config = format!(
            r#"{{"config":{{"Entrypoint":["/bin/tool"],"Env":["A=1"]}},"rootfs":{{"diff_ids":["sha256:{}"]}}}}"#,
            "0".repeat(64)
        );
        write(
            &store.join("oci-configs").join(format!("{hex}.json")),
            &config,
        );
        // Nobody but the agent's user may write the store.
        for dir in ["", "oci-images", "oci-configs"] {
            set_mode(&store.join(dir), 0o755);
        }
        set_mode(&store.join("oci-images").join(&hex), 0o755);
        set_mode(&store.join("oci-images").join(&hex).join("rootfs"), 0o755);
        let host = CrunHost {
            ids: ids.clone(),
            apparmor_userns: root.join("restrict"),
            apparmor_profiles: root.join("profiles"),
            cgroup_mount: root.join("cgroup"),
            self_cgroup: root.join("self_cgroup"),
            id_tool_dirs: vec![tools.clone()],
            owner_uid: fs::metadata(&root).expect("the node").uid(),
        };
        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(host);
        Self {
            dir,
            crun,
            state,
            scratch,
            ids,
            tools,
            cgroups,
        }
    }

    /// The directory of the node.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The seams of the crun runtime over the node: its cgroups, its start-check files and its
    /// id files. Nothing is given away, and each container's supervisor is this test binary,
    /// which answers the helper modes before it runs any case.
    pub fn seams(&self) -> Seams {
        Seams {
            cgroups: CgroupSource::Fixed(CgroupSettings {
                mount: self.cgroups.clone(),
                root: "/root".to_owned(),
                actions: "/actions".to_owned(),
            }),
            hooks: Hooks {
                chown: |_, _, _| Ok(()),
                ..Hooks::default()
            },
            host: current_host,
            ids: IdSources {
                files: self.ids.clone(),
                tool_path: self.tools.clone().into_os_string(),
                check_search: false,
            },
            daemon: std::env::current_exe().expect("the test binary"),
        }
    }

    /// The lines the stand-in appended to `events`.
    pub fn events(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The calls of the stand-in.
    pub fn calls(&self) -> String {
        fs::read_to_string(self.path().join("calls")).unwrap_or_default()
    }

    /// Makes the FIFO the held job `tag` says it is up through, and reads it on a thread: the
    /// receiver gets one message when the stand-in has written it.
    pub fn expect_up(&self, tag: &str) -> Receiver<()> {
        let path = self.path().join(format!("up-{tag}"));
        mknodat(CWD, &path, FileType::Fifo, Mode::from_raw_mode(0o600), 0).expect("the FIFO");
        let (said, up) = channel();
        std::thread::spawn(move || {
            // Opening the FIFO waits for its writer; the line is the stand-in's `up`.
            if fs::read_to_string(&path).is_ok_and(|line| line.trim() == "up") {
                let _ = said.send(());
            }
        });
        up
    }

    /// Lets the held job `tag` end.
    pub fn release(&self, tag: &str) {
        fs::write(self.path().join(format!("go-{tag}")), b"").expect("the go file");
    }
}

/// When this binary runs as a container's supervisor (`args` is `PROGRAM MODE CRUN LEASE NAME
/// ...`, as the crun runtime starts it), makes the container's cgroup where `crun run` would:
/// `ctr` in the lease's cgroup `NAME` below the node's actions cgroup. The stand-in `CRUN` is
/// `NODE/bin/crun`. Any other start is left alone.
pub fn before_supervise(supervise: &str, args: &[OsString]) {
    let [_, mode, crun, _, name, ..] = args else {
        return;
    };
    let node = Path::new(crun).parent().and_then(Path::parent);
    if mode.to_str() == Some(supervise)
        && let Some(node) = node
    {
        let ctr = node.join("runtime-cgroup/actions").join(name).join("ctr");
        fs::create_dir_all(ctr).expect("the container's cgroup");
    }
}
