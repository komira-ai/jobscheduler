//! The agent's `CrunRuntime` on this host, opened as the agent opens it, over a busybox image:
//! what the egress tests (tests/egress) and the launch measurement (`js-launch-measure`,
//! tests/bin/launch_measure.rs) share. Both include this file with `#[path]`. It takes every
//! program, path and id as an argument and reads no environment variable; a step that fails
//! is an error naming the step, never a panic, so the measurement can report it.
//!
//! [`RuntimeHost`] gives the runtime a cgroup tree of its own (this process in its leaf
//! `supervisor`, the leases' cgroups in `actions`), a store that holds a busybox image and its
//! kept config, and a directory `runs` where each run's scratch directory is made. Dropped, it
//! removes every run it holds, moves this process back to the cgroup it came from and removes
//! the tree; each guard is in place before the step it undoes, so a failed setup is undone too.
//! The files under the work directory are the caller's to remove.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use js_agent::crun::{
    CgroupSettings, CrunRuntime, CrunSettings, EgressPort, config_dir, oci_config,
};
use js_agent::egress::{Config, Egress};
use js_agent::lifecycle::{Body, RunSpec, Runtime as _};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_types::JobNetwork;

/// The image source of a store that already holds its one image: a fetch fails.
#[derive(Debug)]
pub struct NoImages;

#[async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed(
            "the image is already in the store".to_owned(),
        ))
    }
}

/// The applets of busybox the jobs use.
const APPLETS: [&str; 18] = [
    "sh", "cat", "id", "grep", "nc", "ip", "sleep", "echo", "wc", "head", "tr", "sed", "ls",
    "date", "true", "false", "nslookup", "test",
];

/// The config of the busybox image of a [`RuntimeHost`]: a run's arguments are a script for
/// `sh -c`.
const IMAGE_CONFIG: &str =
    r#"{"config":{"Entrypoint":["/bin/sh","-c"],"Env":["PATH=/bin"]},"rootfs":{"diff_ids":[]}}"#;

/// `path` and its missing parents made, with `mode`.
fn mkdir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// `result`, with the step that failed and its path named.
fn at<T>(step: &str, path: &Path, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|why| format!("{step} {}: {why}", path.display()))
}

/// Makes a minimal image root at `rootfs`: the static `busybox` copied in, its applets, and
/// the mount points the bundle names.
///
/// # Errors
///
/// The step that failed, with its path.
pub fn busybox_rootfs(rootfs: &Path, busybox: &Path) -> Result<(), String> {
    for dir in [
        "bin", "proc", "dev", "sys", "tmp", "var/tmp", "run", "root", "etc", "work",
    ] {
        let path = rootfs.join(dir);
        at("make", &path, mkdir(&path, 0o755))?;
    }
    for file in ["hosts", "hostname", "resolv.conf"] {
        let path = rootfs.join("etc").join(file);
        at("write", &path, fs::write(&path, ""))?;
    }
    let copy = rootfs.join("bin/busybox");
    at("copy busybox to", &copy, fs::copy(busybox, &copy))?;
    at(
        "make executable",
        &copy,
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)),
    )?;
    for applet in APPLETS {
        let link = rootfs.join("bin").join(applet);
        at("link", &link, symlink("busybox", &link))?;
    }
    Ok(())
}

/// What a [`RuntimeHost`] runs and runs as.
#[derive(Debug, Clone, Copy)]
pub struct Setup<'a> {
    /// The `crun` program.
    pub crun: &'a Path,
    /// A static `busybox`, copied into the image.
    pub busybox: &'a Path,
    /// The job user's uid and gid: every `crun` runs as this user.
    pub job_user: (u32, u32),
    /// The binary that answers the helper modes (`js_contain::modes::dispatch`).
    pub daemon: &'a Path,
    /// Whether each lease's directory is beside its run's scratch directory, as the agent has
    /// it (`OciConfig::leases_in`, `Runtime::lease_dir`): the caller then makes it, empty,
    /// before the run is staged, as the lifecycle does. Otherwise the runtime makes it in the
    /// store.
    pub leases_beside: bool,
}

/// A cgroup subtree this process made and moved into: dropped, it moves the process back to
/// `home` and kills and removes the subtree, deepest first.
struct OwnedCgroup {
    dir: PathBuf,
    home: PathBuf,
}

impl Drop for OwnedCgroup {
    fn drop(&mut self) {
        let _ = fs::write(
            self.home.join("cgroup.procs"),
            std::process::id().to_string(),
        );
        // A killed process leaves its cgroup a moment after the kill.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            remove_cgroup_tree(&self.dir);
            if !self.dir.exists() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Kills what runs in the cgroup `dir` and removes it and the cgroups below it; what is busy
/// stays.
fn remove_cgroup_tree(dir: &Path) {
    let _ = fs::write(dir.join("cgroup.kill"), "1");
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup_tree(&entry.path());
        }
    }
    let _ = fs::remove_dir(dir);
}

/// The cgroup this process is in now (cgroup version 2).
fn own_cgroup() -> Result<PathBuf, String> {
    let own = Path::new("/proc/self/cgroup");
    let text = at("read", own, fs::read_to_string(own))?;
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| format!("{} names no cgroup version 2 path", own.display()))?;
    Ok(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// The agent's `CrunRuntime` on this host; see the module docs.
pub struct RuntimeHost {
    /// The runtime, as the agent opens it.
    pub runtime: CrunRuntime,
    image: String,
    runs: PathBuf,
    /// The network every run asks for: the ceiling of the egress the runtime was opened with
    /// (`none` without one), so a wired runtime's runs are wired.
    network: JobNetwork,
    // Dropped after the runtime: the process goes home and the tree goes.
    cgroup: OwnedCgroup,
}

impl RuntimeHost {
    /// Opens the runtime `host_id` in `work` (an empty directory every user can search, as
    /// are all above it), with the cgroup tree `/js-r-<label>-<pid>`, wired by `egress` (none
    /// in mode none, where a job has loopback only).
    ///
    /// # Errors
    ///
    /// The step that failed.
    pub fn open(
        label: &str,
        host_id: &str,
        work: &Path,
        setup: &Setup<'_>,
        egress: Option<&Egress>,
    ) -> Result<Self, String> {
        let store = work.join("store");
        at("make", &store, mkdir(&store, 0o711))?;
        let runs = work.join("runs");
        at("make", &runs, mkdir(&runs, 0o711))?;
        let hex = "d".repeat(64);
        let rootfs = store.join("oci-images").join(&hex).join("rootfs");
        busybox_rootfs(&rootfs, setup.busybox)?;
        // The runtime binds the run's scratch directory at `/scratch`, and `crun`, run as the
        // job user, cannot make that mount point in a root that root made.
        let scratch = rootfs.join("scratch");
        at("make", &scratch, mkdir(&scratch, 0o755))?;
        let configs = config_dir(&store);
        at("make", &configs, mkdir(&configs, 0o755))?;
        let config = configs.join(format!("{hex}.json"));
        at("write", &config, fs::write(&config, IMAGE_CONFIG))?;

        let name = format!("/js-r-{label}-{}", std::process::id());
        let cgroup = OwnedCgroup {
            dir: PathBuf::from(format!("/sys/fs/cgroup{name}")),
            home: own_cgroup()?,
        };
        let supervisor = cgroup.dir.join("supervisor");
        at("make", &supervisor, mkdir(&supervisor, 0o755))?;
        let control = cgroup.dir.join("cgroup.subtree_control");
        at("write", &control, fs::write(&control, "+cpu +memory +pids"))?;
        let procs = supervisor.join("cgroup.procs");
        at(
            "move this process to",
            &procs,
            fs::write(&procs, std::process::id().to_string()),
        )?;
        let actions = cgroup.dir.join("actions");
        at("make", &actions, mkdir(&actions, 0o755))?;
        let control = actions.join("cgroup.subtree_control");
        at("write", &control, fs::write(&control, "+cpu +memory +pids"))?;

        let egress_config = egress.map_or_else(Config::none, |egress| egress.config().clone());
        let mut oci = oci_config(setup.crun, setup.job_user, &egress_config);
        oci.daemon = setup.daemon.to_owned();
        oci.id_tool_path = Some("/usr/bin:/bin".into());
        if setup.leases_beside {
            oci.leases_in = Some(runs.clone());
        }
        let port = egress.map(|egress| -> Arc<dyn EgressPort> { Arc::new(egress.clone()) });
        let settings = CrunSettings {
            oci,
            scratch: store,
            cgroups: CgroupSettings {
                mount: PathBuf::from("/sys/fs/cgroup"),
                root: name.clone(),
                actions: format!("{name}/actions"),
            },
            host_id: host_id.to_owned(),
            pull_threads: 1,
            files: None,
            egress: port,
            gpus: None,
        };
        let runtime = CrunRuntime::open(settings, Arc::new(NoImages))
            .map_err(|why| format!("open the crun runtime: {why}"))?;
        Ok(Self {
            runtime,
            image: format!("local/busybox@sha256:{hex}"),
            runs,
            network: egress.map_or(JobNetwork::None, |egress| egress.config().mode().level()),
            cgroup,
        })
    }

    /// Moves this process into the runtime's `supervisor` cgroup, the agent's own: `crun`, run
    /// as the job user, moves its first process from there into the container's cgroup, which
    /// the kernel allows only below the runtime's cgroup root. For a process that holds more
    /// than one runtime: [`RuntimeHost::open`] moves it into the runtime it opens.
    ///
    /// # Errors
    ///
    /// The write that failed.
    #[allow(
        dead_code,
        reason = "the egress tests hold one runtime at a time and never call it"
    )]
    pub fn enter(&self) -> Result<(), String> {
        let procs = self.cgroup.dir.join("supervisor/cgroup.procs");
        at(
            "move this process to",
            &procs,
            fs::write(&procs, std::process::id().to_string()),
        )
    }

    /// The run `run_id` of `script` (run by `sh -c` in the image), its scratch directory
    /// made; the job sees it at `/scratch`.
    ///
    /// # Errors
    ///
    /// The scratch directory could not be made.
    pub fn spec(&self, run_id: &str, script: &str) -> Result<RunSpec, String> {
        let scratch = self.runs.join(run_id);
        at("make", &scratch, mkdir(&scratch, 0o700))?;
        Ok(RunSpec {
            run_id: run_id.to_owned(),
            body: Body::Container {
                image: self.image.clone(),
                args: vec![script.to_owned()],
            },
            env: Vec::new(),
            cpu_millis: 1000,
            memory_bytes: 256 << 20,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            scratch,
            files: None,
            slot: 0,
            isolation: js_agent_wire::Isolation::Contained,
            bundle: None,
            labels: BTreeMap::new(),
            network: self.network,
            ports: Vec::new(),
        })
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        for found in self.runtime.list_labelled().unwrap_or_default() {
            let _ = self.runtime.remove(&found.id);
        }
    }
}
