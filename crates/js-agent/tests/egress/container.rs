//! Real containers for the end-to-end tests: js-contain's `Oci` driven by this root process
//! as option B has it (every `crun` runs as the job user, from the distribution's path, with
//! the AppArmor restriction on), the scratch layout of a lease, a busybox root, and the
//! agent's egress calls in the order the runtime makes them.

use std::fs;
use std::future::Future;
use std::os::unix::fs::{PermissionsExt as _, lchown, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use js_contain::container::{ACTION_ID, ContainerLimits, ContainerSpec, Ended};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{Oci, OciConfig, stderr_path, stdout_path};
use js_contain::running::Running;
use js_contain::subids::{DaemonIds, IdFiles, user_ids};

use crate::sys::var;

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

/// Runs a future to its end on a runtime of its own, so a test can hold the lock in plain
/// synchronous code.
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// The job user of the CI job.
pub fn job_user() -> (u32, u32) {
    let (uid, gid): (u32, u32) = (
        var("JS_B_UID").parse().unwrap(),
        var("JS_B_GID").parse().unwrap(),
    );
    assert_ne!(uid, 0, "the job user is not root");
    (uid, gid)
}

pub fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A directory below `/tmp` that every user can search, new and empty.
pub fn world_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    mkdir(&dir, 0o755);
    dir
}

/// The applets of busybox the probes use.
const APPLETS: [&str; 18] = [
    "sh", "cat", "id", "grep", "nc", "ip", "sleep", "echo", "wc", "head", "tr", "sed", "ls",
    "date", "true", "false", "nslookup", "test",
];

/// A minimal image root: busybox and the mount points the bundle names.
fn busybox_rootfs(rootfs: &Path) {
    for dir in [
        "bin", "proc", "dev", "sys", "tmp", "var/tmp", "run", "root", "etc", "work",
    ] {
        mkdir(&rootfs.join(dir), 0o755);
    }
    for file in ["hosts", "hostname", "resolv.conf"] {
        fs::write(rootfs.join("etc").join(file), "").unwrap();
    }
    let busybox = rootfs.join("bin/busybox");
    fs::copy(var("JS_B_BUSYBOX"), &busybox).unwrap();
    fs::set_permissions(&busybox, fs::Permissions::from_mode(0o755)).unwrap();
    for applet in APPLETS {
        symlink("busybox", rootfs.join("bin").join(applet)).unwrap();
    }
}

/// A node of containers: a backend, an image, a delegated cgroup subtree.
pub struct ContainerHost {
    pub oci: Arc<Oci>,
    scratch: PathBuf,
    cgroup_parent: String,
    ids: DaemonIds,
    image: PinnedImage,
    next: AtomicU32,
}

/// How a node differs from the default: the resolvers and the wait for a route that the
/// egress configuration implies.
#[derive(Clone, Default)]
pub struct Network {
    pub resolv_conf: Option<String>,
    pub route_wait: Option<Duration>,
}

impl ContainerHost {
    /// The backend for `label`, with a delegated cgroup subtree and a busybox image.
    pub async fn new(label: &str, network: Network) -> Self {
        let (uid, gid) = job_user();
        let ids = user_ids(&IdFiles::system(), uid, gid).unwrap();
        let work = world_dir(&format!("js-e-{label}"));
        let scratch = work.join("scratch");
        mkdir(&scratch, 0o711);
        let cgroup_parent = format!("/js-e-{label}");
        let mut config = OciConfig {
            runtime: PathBuf::from(var("JS_B_CRUN")),
            id_tool_path: Some("/usr/bin:/bin".into()),
            daemon: std::env::current_exe().unwrap(),
            job_user: Some((uid, gid)),
            route_wait: network.route_wait,
            ..OciConfig::default()
        };
        if let Some(resolv) = network.resolv_conf {
            config.resolv_conf = resolv;
        }
        let hex = "c".repeat(64);
        let image = PinnedImage {
            reference: format!("local/busybox@sha256:{hex}"),
            digest: format!("sha256:{hex}"),
        };
        busybox_rootfs(&scratch.join("oci-images").join(&hex).join("rootfs"));

        let cgroup = PathBuf::from(format!("/sys/fs/cgroup{cgroup_parent}"));
        let _ = fs::remove_dir(&cgroup);
        mkdir(&cgroup.join("supervisor"), 0o755);
        let oci = Oci::new(&config, Arc::new(NoImages), &scratch, &cgroup_parent)
            .unwrap_or_else(|why| panic!("a root daemon starts the backend: {why}"));
        oci.check_pinned(&image).await.unwrap();
        fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids").unwrap();
        delegate(&cgroup, uid, gid);
        fs::write(
            cgroup.join("supervisor/cgroup.procs"),
            std::process::id().to_string(),
        )
        .unwrap();
        Self {
            oci,
            scratch,
            cgroup_parent,
            ids,
            image,
            next: AtomicU32::new(1),
        }
    }

    /// The directories and the spec of a new lease that runs `script` with `sh -c`.
    fn lease(&self, script: &str) -> ContainerSpec {
        let (uid, gid) = job_user();
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("lease{n}");
        let cgroup = PathBuf::from(format!("/sys/fs/cgroup{}", self.cgroup_parent)).join(&name);
        mkdir(&cgroup, 0o755);
        fs::write(cgroup.join("cgroup.subtree_control"), "+memory +pids").unwrap();
        delegate(&cgroup, uid, gid);
        let lease = self.scratch.join(&name);
        mkdir(&lease, 0o755);
        lchown(&lease, Some(uid), Some(gid)).unwrap();
        let (job_uid, job_gid) = (self.ids.sub_uid + ACTION_ID, self.ids.sub_gid + ACTION_ID);
        for dir in [
            "root",
            "upper",
            "work",
            "rw/tmp",
            "rw/var-tmp",
            "rw/run",
            "rw/home",
        ] {
            let path = lease.join(dir);
            mkdir(&path, 0o755);
            lchown(&path, Some(job_uid), Some(job_gid)).unwrap();
        }
        ContainerSpec {
            name: name.clone(),
            image: self.image.reference.clone(),
            image_digest: self.image.digest.clone(),
            dir: lease.clone(),
            cgroup_parent: format!("{}/{name}", self.cgroup_parent),
            exec_root: "/work".to_owned(),
            input_root: lease.join("root"),
            upper: lease.join("upper"),
            overlay_work: lease.join("work"),
            working_directory: String::new(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: ["/bin/sh", "-c", script].map(str::to_owned).to_vec(),
            limits: ContainerLimits::DEFAULT,
            shared: Vec::new(),
            mounts: Vec::new(),
        }
    }

    /// Starts a container that runs `script`, and waits until the backend can name its first
    /// process (the pid the agent wires).
    pub async fn start(&self, script: &str) -> Live {
        let spec = self.lease(script);
        self.oci.create(&spec).await.unwrap();
        let running = self.oci.start(&spec).unwrap();
        let mut pid = None;
        for _ in 0..400 {
            if let Ok(found) = self.oci.container_pid(&spec) {
                pid = Some(found);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Live {
            oci: self.oci.clone(),
            pid: pid.unwrap_or_else(|| {
                panic!(
                    "the container never had a pid: {}",
                    said(&self.oci, &spec, None)
                )
            }),
            spec,
            running,
        }
    }
}

/// A started container.
pub struct Live {
    oci: Arc<Oci>,
    /// The host pid of its first process.
    pub pid: u32,
    pub spec: ContainerSpec,
    running: Running,
}

/// What a container left.
#[derive(Debug)]
pub struct Outcome {
    pub ended: Result<Ended, String>,
    pub stdout: String,
    /// Everything there is to read, for an assertion message.
    pub said: String,
    /// What the container's init said when it could not start the job.
    pub init_report: String,
}

impl Live {
    /// The job's standard output so far.
    pub fn stdout_so_far(&self) -> String {
        fs::read_to_string(stdout_path(&self.spec.dir)).unwrap_or_default()
    }

    /// Waits until the output has a line that starts with `prefix`, for `limit`.
    pub async fn wait_for_line(&self, prefix: &str, limit: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < limit {
            if self
                .stdout_so_far()
                .lines()
                .any(|line| line.starts_with(prefix))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Ends the container with `SIGTERM`.
    pub async fn terminate(&self) {
        let _ = self.oci.terminate(&self.spec.name).await;
    }

    /// Waits for the container to end (a minute at most; then it is killed) and reads what it
    /// left, and removes it.
    pub async fn finish(mut self) -> Outcome {
        if tokio::time::timeout(Duration::from_secs(90), self.running.wait())
            .await
            .is_err()
        {
            let _ = self.oci.terminate(&self.spec.name).await;
            self.running.force().await;
        }
        let ended = self.oci.ended(&self.spec, &self.running).await;
        let stdout = fs::read_to_string(stdout_path(&self.spec.dir)).unwrap_or_default();
        let said = said(&self.oci, &self.spec, Some(&ended));
        let init_report = fs::read_to_string(self.oci.reports(&self.spec).init).unwrap_or_default();
        let _ = self.oci.remove(&self.spec.name);
        Outcome {
            ended,
            stdout,
            said,
            init_report,
        }
    }
}

fn said(oci: &Oci, spec: &ContainerSpec, ended: Option<&Result<Ended, String>>) -> String {
    let reports = oci.reports(spec);
    format!(
        "ended {ended:?}\nstdout:\n{}\nstderr:\n{}\ncrun log:\n{}\ninit report:\n{}",
        fs::read_to_string(stdout_path(&spec.dir)).unwrap_or_default(),
        fs::read_to_string(stderr_path(&spec.dir)).unwrap_or_default(),
        fs::read_to_string(&reports.log).unwrap_or_default(),
        fs::read_to_string(&reports.init).unwrap_or_default()
    )
}

/// Lets the job user manage the cgroup subtree `dir`, as a unit's `Delegate=` does.
fn delegate(dir: &Path, uid: u32, gid: u32) {
    for name in [
        "",
        "cgroup.procs",
        "cgroup.threads",
        "cgroup.subtree_control",
    ] {
        lchown(dir.join(name), Some(uid), Some(gid)).unwrap();
    }
}

/// The names of the interfaces in a `/proc/net/dev` listing.
pub fn interfaces(dev: &str) -> Vec<String> {
    dev.lines()
        .skip(2)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_owned())
        .collect()
}

/// The lines of `stdout` after the line `--<name>` and before the next line that starts with
/// `--`.
pub fn section(stdout: &str, name: &str) -> String {
    let marker = format!("--{name}");
    stdout
        .lines()
        .skip_while(|line| *line != marker)
        .skip(1)
        .take_while(|line| !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The shell function every probe script defines: `p HOST PORT` prints
/// `reach HOST:PORT [what nc said]`, and what the server says (`pong`) when it connected.
pub const PROBE: &str =
    "p() { o=$(nc -w 2 \"$1\" \"$2\" </dev/null 2>&1); echo \"reach $1:$2 [$o]\"; }\n";
