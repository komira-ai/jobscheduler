//! An OCI bundle for one probe container and the `crun` calls that run it.
//!
//! The root file system is a directory of mount points: the host's `/usr` bound read-only
//! (with `lib`, `lib64`, `bin` and `sbin` as links into it, as on the merged-usr runner),
//! `/proc`, a tmpfs `/dev` and `/tmp`, and this test binary bound at `/probe`. Container id
//! 0 is the first id of the runner's subordinate range, as the job runner's containers do
//! (`crates/js-contain/src/oci/mod.rs`), and the job runs as id 1000 with no capabilities and
//! `no_new_privs`. No seccomp filter unless `Spec::seccomp` asks for the real one, and no
//! cgroup limits (not under test; the
//! driver runs `crun` with `--cgroup-manager=disabled`).

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use js_testkit::spawn::install_copy;
use serde_json::{Value, json};

use crate::host;

/// How a namespace is made.
#[derive(Clone, Debug)]
pub enum Join {
    /// A new one.
    New,
    /// The existing one at this path.
    Path(String),
    /// None: the host's own (only meaningful for the user namespace, with a root daemon).
    Host,
}

/// What one container is.
#[derive(Clone, Debug)]
pub struct Spec {
    /// A name for the bundle directory and the container id.
    pub name: String,
    /// The probe command line (`JS_EGRESS_PROBE`).
    pub probe: String,
    /// Extra environment.
    pub env: Vec<(String, String)>,
    /// The job's id inside the container.
    pub uid: u32,
    /// Give the process `CAP_NET_ADMIN` and no `no_new_privs` (the control of the tamper tests).
    pub net_admin: bool,
    /// The network namespace.
    pub net: Join,
    /// The user namespace.
    pub user: Join,
    /// The AppArmor profile the process changes to.
    pub apparmor: Option<String>,
    /// Make the probe wait for a file in `/gate` before it exits.
    pub gate: bool,
    /// The `crun` program to run, when not the distribution's.
    pub crun: Option<PathBuf>,
    /// Apply the container runner's seccomp profile (no capabilities).
    pub seccomp: bool,
    /// Run `crun` as root (`sudo -n`), as the root agent of option B does.
    pub root: bool,
}

impl Spec {
    /// A job: id 1000, no capabilities, new namespaces.
    pub fn new(name: &str, probe: &str) -> Self {
        Self {
            name: name.to_owned(),
            probe: probe.to_owned(),
            env: Vec::new(),
            uid: 1000,
            net_admin: false,
            net: Join::New,
            user: Join::New,
            apparmor: None,
            gate: false,
            crun: None,
            seccomp: false,
            root: false,
        }
    }
}

/// What a container printed and how it ended.
#[derive(Debug)]
pub struct Outcome {
    /// `crun`'s exit status (the container's, or `crun`'s own failure).
    pub exit: Option<i32>,
    /// The container's standard output.
    pub stdout: String,
    /// The container's standard error, with `crun`'s.
    pub stderr: String,
    /// `crun`'s own log.
    pub log: String,
}

impl Outcome {
    /// The `PROBE key=value` lines of the output.
    pub fn probe(&self) -> BTreeMap<String, String> {
        parse_probe(&self.stdout)
    }

    /// The value of one probe key, or `-` when the probe never printed it.
    pub fn get(&self, key: &str) -> String {
        self.probe()
            .get(key)
            .cloned()
            .unwrap_or_else(|| "-".to_owned())
    }

    /// The first line of the container's or `crun`'s errors, for the report.
    pub fn first_error(&self) -> String {
        self.stderr
            .lines()
            .chain(self.log.lines())
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_owned()
    }
}

/// The `PROBE key=value` lines of `text`.
pub fn parse_probe(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once("PROBE ")?.1.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The directory every bundle of this test process is under, searchable by everyone (the
/// container's ids are not the runner's).
fn base_dir() -> PathBuf {
    let base = std::env::temp_dir().join(format!("js-egress-spike-{}", std::process::id()));
    let _ = std::fs::DirBuilder::new().mode(0o755).create(&base);
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).expect("chmod base");
    base
}

/// The subordinate range of the runner user in `file` (`/etc/subuid` or `/etc/subgid`).
fn subrange(file: &str) -> (u32, u32) {
    let uid = rustix::process::getuid().as_raw().to_string();
    let user = std::env::var("USER").unwrap_or_default();
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            let owner = fields.next()?;
            let start = fields.next()?.parse().ok()?;
            let count = fields.next()?.parse().ok()?;
            (owner == user || owner == uid).then_some((start, count))
        })
        .unwrap_or_else(|| panic!("no range for user {user:?} in {file}"))
}

/// The first subordinate uid and gid of the runner user.
pub fn sub_ids() -> (u32, u32) {
    (subrange("/etc/subuid").0, subrange("/etc/subgid").0)
}

/// A bundle on disk.
#[derive(Debug)]
pub struct Bundle {
    /// The bundle directory.
    pub dir: PathBuf,
    id: String,
    spec: Spec,
}

fn mkdir(path: &Path) {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

impl Bundle {
    /// Writes the bundle of `spec`.
    pub fn new(spec: &Spec) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base_dir().join(format!("{}-{n}", spec.name));
        let id = format!(
            "jsx{}x{}x{n}",
            std::process::id(),
            spec.name.replace(['_', '-'], "")
        );
        let root = dir.join("rootfs");
        for sub in ["proc", "dev", "tmp", "usr", "etc", "gate"] {
            mkdir(&root.join(sub));
        }
        std::fs::write(root.join("etc/ld.so.cache"), b"").expect("mount point file");
        std::fs::write(root.join("probe"), b"").expect("mount point file");
        for (link, target) in [
            ("bin", "usr/bin"),
            ("sbin", "usr/sbin"),
            ("lib", "usr/lib"),
            ("lib64", "usr/lib64"),
        ] {
            symlink(target, root.join(link)).expect("link");
        }
        mkdir(&dir.join("state"));
        std::fs::set_permissions(dir.join("state"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
        mkdir(&dir.join("gate"));
        install_copy(
            &std::env::current_exe().expect("own path"),
            &dir.join("probe-bin"),
            "755",
        );
        let bundle = Self {
            dir,
            id,
            spec: spec.clone(),
        };
        std::fs::write(
            bundle.dir.join("config.json"),
            serde_json::to_vec_pretty(&bundle.config()).expect("json"),
        )
        .expect("config.json");
        bundle
    }

    fn config(&self) -> Value {
        let spec = &self.spec;
        let mut env = vec![
            "PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
            "HOME=/tmp".to_owned(),
            "RUST_BACKTRACE=0".to_owned(),
            format!("JS_EGRESS_PROBE={}", spec.probe),
        ];
        if spec.gate {
            env.push("JS_EGRESS_GATE=/gate/release".to_owned());
        }
        env.extend(spec.env.iter().map(|(k, v)| format!("{k}={v}")));
        let caps: Vec<&str> = if spec.net_admin {
            vec!["CAP_NET_ADMIN"]
        } else {
            vec![]
        };
        let mut process = json!({
            "terminal": false,
            "user": {"uid": spec.uid, "gid": spec.uid},
            "args": ["/probe", "--exact", "probe_entry", "--ignored", "--nocapture", "--test-threads=1"],
            "env": env,
            "cwd": "/",
            "capabilities": {
                "bounding": caps, "effective": caps, "inheritable": [], "permitted": caps, "ambient": []
            },
            "noNewPrivileges": !spec.net_admin,
        });
        if let Some(profile) = &spec.apparmor {
            process["apparmorProfile"] = json!(profile);
        }
        let bind = |dest: &str, source: &str, options: &[&str]| json!({"destination": dest, "type": "bind", "source": source, "options": options});
        let mut mounts = vec![
            json!({"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]}),
            json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]}),
            json!({"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777", "size=65536k"]}),
            bind("/usr", "/usr", &["rbind", "ro"]),
            bind("/etc/ld.so.cache", "/etc/ld.so.cache", &["bind", "ro"]),
            bind(
                "/probe",
                &self.dir.join("probe-bin").to_string_lossy(),
                &["bind", "ro"],
            ),
            bind(
                "/gate",
                &self.dir.join("gate").to_string_lossy(),
                &["rbind", "ro"],
            ),
        ];
        for device in ["null", "zero", "urandom", "random", "full"] {
            let path = format!("/dev/{device}");
            mounts.push(bind(&path, &path, &["rbind", "nosuid", "noexec"]));
        }
        let mut namespaces = vec![
            json!({"type": "pid"}),
            json!({"type": "ipc"}),
            json!({"type": "uts"}),
            json!({"type": "mount"}),
        ];
        namespaces.push(match &spec.net {
            Join::New | Join::Host => json!({"type": "network"}),
            Join::Path(path) => json!({"type": "network", "path": path}),
        });
        let mut linux = json!({});
        match &spec.user {
            Join::New => {
                namespaces.push(json!({"type": "user"}));
                let (uid, gid) = sub_ids();
                linux["uidMappings"] = json!([{"containerID": 0, "hostID": uid, "size": 65536}]);
                linux["gidMappings"] = json!([{"containerID": 0, "hostID": gid, "size": 65536}]);
            }
            Join::Path(path) => namespaces.push(json!({"type": "user", "path": path})),
            Join::Host => {}
        }
        linux["namespaces"] = json!(namespaces);
        if spec.seccomp {
            linux["seccomp"] = crate::seccomp::section().0;
        }
        json!({
            "ociVersion": "1.0.2",
            "process": process,
            "root": {"path": "rootfs", "readonly": true},
            "mounts": mounts,
            "linux": linux,
        })
    }

    fn crun(&self) -> Command {
        let crun = self
            .spec
            .crun
            .clone()
            .unwrap_or_else(|| PathBuf::from(host::bin("crun")));
        let mut cmd = if self.spec.root {
            let mut sudo = Command::new("sudo");
            sudo.arg("-n").arg(crun);
            sudo
        } else {
            Command::new(crun)
        };
        cmd.arg("--root")
            .arg(self.dir.join("state"))
            .arg("--cgroup-manager=disabled")
            .arg("--log")
            .arg(self.dir.join("crun.log"));
        cmd
    }

    fn files(&self) -> (std::fs::File, std::fs::File) {
        let open = |name: &str| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(name))
                .expect("output file")
        };
        (open("out.txt"), open("err.txt"))
    }

    fn outcome(&self, exit: Option<i32>) -> Outcome {
        let read = |name: &str| std::fs::read_to_string(self.dir.join(name)).unwrap_or_default();
        Outcome {
            exit,
            stdout: read("out.txt"),
            stderr: read("err.txt"),
            log: read("crun.log"),
        }
    }

    /// `crun run`: the container runs to its end (at most 120 s).
    pub fn run(&self) -> Outcome {
        let (out, err) = self.files();
        let mut cmd = self.crun();
        cmd.arg("run").arg("--bundle").arg(&self.dir).arg(&self.id);
        cmd.stdout(out).stderr(err);
        let status = wait_timeout(&mut cmd, Duration::from_secs(120));
        let outcome = self.outcome(status);
        self.delete();
        outcome
    }

    /// `crun create`: the container exists with its namespaces and has not run its program.
    pub fn create(self) -> Result<Created, Outcome> {
        let (out, err) = self.files();
        let mut cmd = self.crun();
        cmd.arg("create")
            .arg("--bundle")
            .arg(&self.dir)
            .arg("--pid-file")
            .arg(self.dir.join("pid"))
            .arg(&self.id);
        cmd.stdout(out).stderr(err);
        let status = wait_timeout(&mut cmd, Duration::from_secs(60));
        let pid = std::fs::read_to_string(self.dir.join("pid"))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        match (status, pid) {
            (Some(0), Some(pid)) => Ok(Created { bundle: self, pid }),
            _ => {
                let outcome = self.outcome(status);
                self.delete();
                Err(outcome)
            }
        }
    }

    fn delete(&self) {
        let mut cmd = self.crun();
        cmd.arg("delete").arg("--force").arg(&self.id);
        host::run_timeout(cmd, Duration::from_secs(30));
    }
}

/// Runs `cmd` to its end or kills it at `limit`; its exit status (128 + signal when killed).
fn wait_timeout(cmd: &mut Command, limit: Duration) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|error| panic!("cannot start {cmd:?}: {error}"));
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().or_else(|| status.signal().map(|s| 128 + s)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A created container: namespaces made, program not started.
#[derive(Debug)]
pub struct Created {
    bundle: Bundle,
    /// The pid of the container's init, in its namespaces, as the host sees it.
    pub pid: u32,
}

impl Created {
    /// Starts the program.
    pub fn start(&self) {
        let mut cmd = self.bundle.crun();
        cmd.arg("start").arg(&self.bundle.id);
        let output = host::run_timeout(cmd, Duration::from_secs(30));
        assert!(
            output.status.success(),
            "crun start: {}",
            host::text(&output)
        );
    }

    /// Waits until the program has printed `PROBE <key>=`; false at the limit.
    pub fn wait_for(&self, key: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let needle = format!("PROBE {key}=");
        while Instant::now() < deadline {
            let out = std::fs::read_to_string(self.bundle.dir.join("out.txt")).unwrap_or_default();
            if out.contains(&needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Lets a gated probe finish, waits for the container to stop and returns what it printed.
    pub fn finish(&self) -> Outcome {
        let _ = std::fs::write(self.bundle.dir.join("gate/release"), b"");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let mut cmd = self.bundle.crun();
            cmd.arg("state").arg(&self.bundle.id);
            let output = host::run_timeout(cmd, Duration::from_secs(15));
            if !String::from_utf8_lossy(&output.stdout).contains("\"running\"") {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.bundle.outcome(None)
    }

    /// What the container has printed so far.
    pub fn so_far(&self) -> Outcome {
        self.bundle.outcome(None)
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        self.bundle.delete();
    }
}

/// A copy of the distribution's `crun` at a path no AppArmor profile is attached to.
pub fn crun_copy() -> PathBuf {
    let path = base_dir().join("crun-copy");
    if !path.exists() {
        // Made under another name and renamed, so that no test finds it half written.
        let staged = path.with_extension(std::process::id().to_string());
        install_copy(Path::new(&host::bin("crun")), &staged, "755");
        std::fs::rename(&staged, &path).expect("publish the copy");
    }
    path
}
