//! One test's runtime: a scratch directory, a cgroup under the unit's delegated one and an
//! [`Oci`] over the real `crun`, run by this (root) process with the job user's ids for the
//! container.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, lchown, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use js_contain::cgroup::{LeaseCgroup, LeasePolicy};
use js_contain::container::{ACTION_ID, ContainerLimits, ContainerSpec, Ended, HostMount};
use js_contain::delegate::{self, Delegation, PROC_SELF_CGROUP};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{Oci, OciConfig, stderr_path, stdout_path};
use js_contain::running::Running;
use js_contain::subids::{DaemonIds, IdFiles, user_ids};
use js_testkit::spawn::install_copy;
use js_types::Resources;

/// The cgroup v2 mount.
pub const MOUNT: &str = "/sys/fs/cgroup";

/// The programs a job may run: each a link to the busybox of the image root.
const APPLETS: [&str; 22] = [
    "sh", "cat", "grep", "id", "ls", "touch", "mkdir", "cp", "chmod", "tail", "head", "true",
    "unshare", "mkfifo", "wc", "sed", "cut", "tr", "sort", "env", "stat", "readlink",
];

/// The files that make a cgroup directory the job user's to manage (a unit's `Delegate=`
/// for a daemon that is not root): the directory itself, and the files a process may be
/// moved with and controllers enabled through.
const OWNED_FILES: [&str; 4] = [
    "",
    "cgroup.procs",
    "cgroup.threads",
    "cgroup.subtree_control",
];

/// The value of the environment variable `name`, which `cargo xtask crun-tests` sets.
pub fn var(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set: run through cargo xtask crun-tests"))
}

/// The job user's uid and gid: the account `JS_JOB_USER` names, from `/etc/passwd`.
pub fn job_user() -> (u32, u32) {
    let name = var("JS_JOB_USER");
    let passwd = fs::read_to_string("/etc/passwd").expect("read /etc/passwd");
    let ids = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        match fields[..] {
            [user, _, uid, gid, ..] if user == name => Some((uid.parse().ok()?, gid.parse().ok()?)),
            _ => None,
        }
    });
    let (uid, gid) = ids.unwrap_or_else(|| panic!("no user {name} in /etc/passwd"));
    assert!(uid != 0 && gid != 0, "the job user {name} is not root");
    (uid, gid)
}

/// The first `program` on the `PATH`.
fn find_on_path(program: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("no {program} on the PATH: run through cargo xtask crun-tests"))
}

/// Makes `path` (and its parents) a directory with `mode`.
pub fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).expect("make a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set its mode");
}

/// The directory of the cgroup `name` (relative to the mount, starting with `/`).
fn cgroup_dir(name: &str) -> PathBuf {
    Path::new(MOUNT).join(name.trim_start_matches('/'))
}

/// Gives the job user `files` of the cgroup directory `dir` ([`OWNED_FILES`] or some of
/// them).
fn give_cgroup(dir: &Path, files: &[&str], uid: u32, gid: u32) {
    for file in files {
        let path = dir.join(file);
        lchown(&path, Some(uid), Some(gid))
            .unwrap_or_else(|e| panic!("give {} to the job user: {e}", path.display()));
    }
}

/// The unit's cgroup, set up by the code a daemon runs, once per test process, and handed
/// to the job user as far as `crun` (run as that user) needs: it makes a lease's container
/// cgroup below the lease's, and moves its first process there from the daemon's own leaf,
/// which the kernel allows only with write access to the `cgroup.procs` of their common
/// ancestor, the unit's cgroup.
fn delegation() -> &'static Delegation {
    static DELEGATION: OnceLock<Delegation> = OnceLock::new();
    DELEGATION.get_or_init(|| {
        let own = fs::read_to_string(PROC_SELF_CGROUP).expect("read /proc/self/cgroup");
        let delegation = delegate::delegate(Path::new(MOUNT), &own, 0, None).unwrap_or_else(|e| {
            panic!("set up the delegated cgroup (run through cargo xtask crun-tests): {e}")
        });
        let (uid, gid) = job_user();
        give_cgroup(&cgroup_dir(&delegation.root), &["cgroup.procs"], uid, gid);
        give_cgroup(&cgroup_dir(&delegation.actions), &OWNED_FILES, uid, gid);
        delegation
    })
}

/// Removes the cgroup `dir` and the cgroups below it, deepest first; what is still busy
/// stays.
fn remove_cgroup_tree(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_cgroup_tree(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

/// An image source for a store that already holds the image: it is never asked.
#[derive(Debug)]
struct NoImages;

#[async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed(
            "the image is already in the store".to_owned(),
        ))
    }
}

/// A minimal image root: the static busybox, its applets, and the mount points the bundle
/// names (`crun` runs as the job user, which cannot make them in a root-owned image).
fn busybox_rootfs(rootfs: &Path) {
    for dir in [
        "bin", "proc", "dev", "sys", "tmp", "var/tmp", "run", "root", "etc", "work", "sync",
    ] {
        mkdir(&rootfs.join(dir), 0o755);
    }
    for file in ["hosts", "hostname", "resolv.conf"] {
        fs::write(rootfs.join("etc").join(file), "").expect("an etc file");
    }
    let busybox = rootfs.join("bin/busybox");
    install_copy(&find_on_path("busybox"), &busybox, "755");
    for applet in APPLETS {
        symlink("busybox", rootfs.join("bin").join(applet)).expect("an applet link");
    }
}

/// The backend's configuration for a root daemon: the distribution's `crun` (AppArmor
/// attaches its profile to that path, which lets the job user's `crun` make the user
/// namespace), and the job user.
fn config(job: (u32, u32)) -> OciConfig {
    OciConfig {
        runtime: PathBuf::from("/usr/bin/crun"),
        id_tool_path: Some("/usr/bin:/bin".into()),
        daemon: PathBuf::from(env!("CARGO_BIN_EXE_js-helper")),
        job_user: Some(job),
        ..OciConfig::default()
    }
}

/// What a lease is given besides the defaults.
pub struct Shape {
    /// The limits written into the bundle.
    pub limits: ContainerLimits,
    /// The lease's memory booking, which is its `memory.max` (the hard policy), in bytes.
    pub memory: u64,
    /// Host directories bound into the container.
    pub mounts: Vec<HostMount>,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            limits: ContainerLimits::DEFAULT,
            memory: 512 << 20,
            mounts: Vec::new(),
        }
    }
}

/// One lease: its container's spec and cgroup.
pub struct Lease {
    /// What `Oci` runs.
    pub spec: ContainerSpec,
    /// The lease's cgroup, with the booking's `memory.max`.
    pub cgroup: LeaseCgroup,
    /// The lease's `memory.max`.
    pub cap: u64,
}

/// How a container ended and what it wrote.
pub struct Outcome {
    /// How `crun run` ended.
    pub ended: Result<Ended, String>,
    /// The job's standard output.
    pub stdout: String,
    /// The job's standard error.
    pub stderr: String,
    /// Everything there is to read about the run, for an assertion message.
    pub said: String,
}

impl Outcome {
    /// Whether the job ran and exited 0.
    pub fn succeeded(&self) -> bool {
        matches!(self.ended, Ok(Ended::Exited(0)))
    }

    /// The job's standard output, after asserting that it exited 0.
    pub fn out(&self) -> &str {
        assert!(self.succeeded(), "{}", self.said);
        &self.stdout
    }
}

/// One test's scratch directory, cgroup and runtime.
pub struct Cell {
    /// The runtime, over the real `crun`, run as root with the job user's ids.
    pub oci: Arc<Oci>,
    /// The scratch directory, under `/tmp`, which every id can search.
    pub scratch: PathBuf,
    /// The cgroup the cell's leases are made under, relative to the mount.
    pub cgroup: String,
    /// The job user's ids and subordinate ranges.
    pub ids: DaemonIds,
    /// The image the leases run.
    pub image: PinnedImage,
}

impl Cell {
    /// A cell named `label`, with the busybox image in its store.
    pub fn new(label: &str) -> Self {
        Self::build(label, Arc::new(NoImages), true)
    }

    /// A cell named `label` whose store is empty: the image is pulled from `source` by the
    /// real helper container.
    pub fn pulling(label: &str, source: Arc<dyn ImageSource>) -> Self {
        Self::build(label, source, false)
    }

    /// The image store of the cell.
    pub fn store(&self) -> PathBuf {
        self.scratch.join("oci-images")
    }

    fn build(label: &str, source: Arc<dyn ImageSource>, seeded: bool) -> Self {
        let (uid, gid) = job_user();
        let ids = user_ids(&IdFiles::system(), uid, gid)
            .unwrap_or_else(|e| panic!("the job user needs a subordinate id range: {e}"));
        let tag = format!("{label}-{}", std::process::id());
        let scratch = PathBuf::from(format!("/tmp/js-crun-root-{tag}"));
        let _ = fs::remove_dir_all(&scratch);
        mkdir(&scratch, 0o711);
        let cgroup = format!("{}/{tag}", delegation().actions);
        let dir = cgroup_dir(&cgroup);
        fs::create_dir(&dir).expect("create the cell's cgroup");
        fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        give_cgroup(&dir, &OWNED_FILES, uid, gid);
        let oci = Oci::new(&config((uid, gid)), source, &scratch, &cgroup)
            .unwrap_or_else(|e| panic!("a root daemon starts the backend: {e}"));
        let hex = if seeded { "c" } else { "d" }.repeat(64);
        if seeded {
            let store = scratch.join("oci-images");
            mkdir(&store, 0o755);
            mkdir(&store.join(&hex), 0o755);
            busybox_rootfs(&store.join(&hex).join("rootfs"));
        }
        Self {
            oci,
            scratch,
            cgroup,
            ids,
            image: PinnedImage {
                reference: format!("local/busybox@sha256:{hex}"),
                digest: format!("sha256:{hex}"),
            },
        }
    }

    /// The host id the job runs as: subordinate id [`ACTION_ID`] of the job user's range.
    pub fn job_ids(&self) -> (u32, u32) {
        (self.ids.sub_uid + ACTION_ID, self.ids.sub_gid + ACTION_ID)
    }

    /// A directory of named pipes every id may open, to be bound at `/sync` of the leases
    /// that must meet: the open of a named pipe blocks until the other end is opened, so
    /// no lease passes a step before the other has reached it.
    pub fn sync_dir(&self, name: &str, pipes: &[&str]) -> HostMount {
        let dir = self.scratch.join(name);
        mkdir(&dir, 0o777);
        for pipe in pipes {
            let path = dir.join(pipe);
            let mode = rustix::fs::Mode::from_raw_mode(0o666);
            rustix::fs::mknodat(rustix::fs::CWD, &path, rustix::fs::FileType::Fifo, mode, 0)
                .unwrap_or_else(|e| panic!("make the pipe {}: {e}", path.display()));
            fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("pipe mode");
        }
        HostMount {
            target: "/sync".to_owned(),
            source: dir,
            read_only: false,
        }
    }

    /// The lease `name` running `argv` over the image, with its directories handed to the
    /// job's id (as a root daemon does itself, with no helper container) and its cgroup
    /// made, limited as `shape` says and handed to the job user.
    pub fn lease(&self, name: &str, argv: &[&str], shape: Shape) -> Lease {
        let (uid, gid) = job_user();
        let (job_uid, job_gid) = self.job_ids();
        let dir = self.scratch.join(name);
        mkdir(&dir, 0o755);
        lchown(&dir, Some(uid), Some(gid)).expect("give the lease to the job user");
        for part in ["root", "upper", "work", "rw/home"] {
            let path = dir.join(part);
            mkdir(&path, 0o755);
            lchown(&path, Some(job_uid), Some(job_gid)).expect("give a directory to the job");
        }
        let cgroup = LeaseCgroup::at(Path::new(MOUNT), &self.cgroup, name);
        let booking = Resources::new(1000, shape.memory, 0, None).expect("a booking");
        cgroup
            .create_with(&booking, None, LeasePolicy::Hard)
            .unwrap_or_else(|e| panic!("create the lease cgroup: {e}"));
        give_cgroup(cgroup.dir(), &OWNED_FILES, uid, gid);
        let spec = ContainerSpec {
            name: name.to_owned(),
            image: self.image.reference.clone(),
            image_digest: self.image.digest.clone(),
            dir: dir.clone(),
            cgroup_parent: cgroup.name().to_owned(),
            exec_root: "/work".to_owned(),
            input_root: dir.join("root"),
            upper: dir.join("upper"),
            overlay_work: dir.join("work"),
            working_directory: String::new(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            limits: shape.limits,
            shared: Vec::new(),
            mounts: shape.mounts,
        };
        Lease {
            spec,
            cgroup,
            cap: shape.memory,
        }
    }

    /// Writes the bundle of `lease` and starts its container.
    pub async fn start(&self, lease: &Lease) -> Running {
        self.oci
            .create(&lease.spec)
            .await
            .unwrap_or_else(|e| panic!("write the bundle: {e}"));
        self.oci
            .start(&lease.spec)
            .unwrap_or_else(|e| panic!("start the container: {e}"))
    }

    /// Waits for the container to end, reads what it wrote and removes the container.
    pub async fn finish(&self, lease: &Lease, mut running: Running) -> Outcome {
        running.wait().await;
        let spec = &lease.spec;
        let ended = self.oci.ended(spec, &running).await;
        let stdout = fs::read_to_string(stdout_path(&spec.dir)).unwrap_or_default();
        let stderr = fs::read_to_string(stderr_path(&spec.dir)).unwrap_or_default();
        let reports = self.oci.reports(spec);
        let said = format!(
            "{}: ended {ended:?}\nstdout:\n{stdout}\nstderr:\n{stderr}\ncrun log:\n{}\ninit \
             report:\n{}",
            spec.name,
            fs::read_to_string(&reports.log).unwrap_or_default(),
            fs::read_to_string(&reports.init).unwrap_or_default()
        );
        let _ = self.oci.remove(&spec.name);
        Outcome {
            ended,
            stdout,
            stderr,
            said,
        }
    }

    /// Runs the shell script `script` as the lease `name` with the default shape.
    pub async fn sh(&self, name: &str, script: &str) -> Outcome {
        let lease = self.lease(name, &["/bin/sh", "-c", script], Shape::default());
        let running = self.start(&lease).await;
        self.finish(&lease, running).await
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        remove_cgroup_tree(&cgroup_dir(&self.cgroup));
        let _ = fs::remove_dir_all(&self.scratch);
    }
}
