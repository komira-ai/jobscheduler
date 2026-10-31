//! The subject of the suite: a `CrunRuntime` over the real `crun`, with the directories,
//! the cgroup and the image of one test.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use js_agent::crun::{CgroupSettings, CrunRuntime, CrunSettings, config_dir};
use js_agent::image::RegistryImages;
use js_agent::lifecycle::{
    Capacity, Config, DirLayout, MountRoots, Prepared, RunHandle, RunSpec, ScratchRoot, StateRoot,
};
use js_contain::OciConfig;
use js_contain::delegate::{self, Delegation, PROC_SELF_CGROUP};
use js_oci::{Allow, Client, Policy};

use crate::runtime_suite::{self, Program, Shared, Subject};

/// The cgroup v2 mount.
const MOUNT: &str = "/sys/fs/cgroup";

/// What a test's job may not use of the node: below a hosted runner's memory.
const BUSY_NODE_LIMIT: u64 = 192 << 20;

/// The value of the variable `name`, which `cargo xtask crun-tests` sets.
fn var(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set: run through cargo xtask crun-tests"))
}

/// The job user's uid and gid: the account `JS_JOB_USER` names, from `/etc/passwd`.
fn job_user() -> (u32, u32) {
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

/// The unit's cgroup, set up by the code a daemon runs, once per test process.
fn delegation() -> &'static Delegation {
    static DELEGATION: OnceLock<Delegation> = OnceLock::new();
    DELEGATION.get_or_init(|| {
        let own = fs::read_to_string(PROC_SELF_CGROUP).expect("read /proc/self/cgroup");
        delegate::delegate(Path::new(MOUNT), &own, 0, None)
            .unwrap_or_else(|e| panic!("set up the delegated cgroup: {e}"))
    })
}

/// Makes `path` (and its parents) a directory with `mode`.
fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).expect("make a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set its mode");
}

/// The directory of the cgroup `name` (relative to the mount, starting with `/`).
fn cgroup_dir(name: &str) -> PathBuf {
    Path::new(MOUNT).join(name.trim_start_matches('/'))
}

/// Kills what runs in the cgroup `dir` and removes it and the cgroups below it; what is
/// still busy stays.
fn remove_cgroup_tree(dir: &Path) {
    let _ = fs::write(dir.join("cgroup.kill"), "1");
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_cgroup_tree(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

/// Docker Hub's library, through the agent's registry client.
fn registry(scratch: &Path) -> Arc<RegistryImages> {
    let policy = Policy::new(vec![Allow::parse("docker.io/library/").expect("an entry")])
        .allow_realm("auth.docker.io")
        .expect("a realm");
    let images = RegistryImages::new(Client::new(policy), std::env::consts::ARCH)
        .expect("an image source for this machine")
        .keeping_configs(config_dir(scratch));
    Arc::new(images)
}

/// One test's directories, cgroup and runtime.
pub struct CrunSubject {
    base: PathBuf,
    cgroup: String,
    image: String,
    crun: Rc<RefCell<CrunRuntime>>,
}

/// A subject for the test `name`.
pub fn crun_subject(name: &str) -> CrunSubject {
    let (uid, gid) = job_user();
    let tag = format!("{}-{}", &name[..name.len().min(24)], std::process::id());
    let base = PathBuf::from(format!("/tmp/js-crun-runtime-{tag}"));
    let _ = fs::remove_dir_all(&base);
    mkdir(&base, 0o711);
    // The leases' cgroup must be named `actions` (the runtime refuses another name), so
    // each test has a cgroup of its own with an `actions` below it.
    let own = format!("{}/{tag}", delegation().actions);
    let cgroup = format!("{own}/actions");
    for dir in [&own, &cgroup] {
        fs::create_dir(cgroup_dir(dir)).expect("create the test's cgroup");
        fs::write(
            cgroup_dir(dir).join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        )
        .expect("enable controllers");
    }
    let reference = var("JS_TEST_IMAGE");
    let image = reference
        .strip_prefix("docker://")
        .unwrap_or(&reference)
        .to_owned();
    let scratch = base.join("oci");
    let settings = CrunSettings {
        oci: OciConfig {
            runtime: PathBuf::from("/usr/bin/crun"),
            id_tool_path: Some("/usr/bin:/bin".into()),
            daemon: PathBuf::from(env!("CARGO_BIN_EXE_js-agent")),
            job_user: Some((uid, gid)),
            ..OciConfig::default()
        },
        scratch: scratch.clone(),
        cgroups: CgroupSettings {
            mount: PathBuf::from(MOUNT),
            root: delegation().root.clone(),
            actions: cgroup.clone(),
        },
        host_id: "crun-suite".to_owned(),
        pull_threads: 1,
        files: None,
    };
    let crun =
        CrunRuntime::open(settings, registry(&scratch)).unwrap_or_else(|e| panic!("open: {e}"));
    CrunSubject {
        base,
        cgroup,
        image,
        crun: Rc::new(RefCell::new(crun)),
    }
}

impl CrunSubject {
    /// A directory below the subject's, searchable by the job's ids.
    fn made(&self, parts: &[&str], mode: u32) -> PathBuf {
        let path = parts.iter().fold(self.base.clone(), |p, part| p.join(part));
        mkdir(&path, mode);
        path
    }
}

impl Subject for CrunSubject {
    fn runtime(&self) -> Shared {
        self.crun.clone()
    }

    fn host_id(&self) -> String {
        "crun-suite".to_owned()
    }

    fn image(&self) -> String {
        self.image.clone()
    }

    fn prepared(&mut self) {
        let prepared = self.crun.borrow().wait_prepared(&self.image);
        assert_eq!(prepared, Prepared::Ready);
    }

    fn scratch(&mut self, run_id: &str) -> PathBuf {
        self.made(&["runs"], 0o711);
        self.made(&["runs", run_id], 0o700)
    }

    fn sync(&mut self, run_id: &str) -> PathBuf {
        self.made(&["sync"], 0o711);
        let dir = self.made(&["sync", run_id], 0o777);
        let ready = dir.join("ready");
        if !ready.exists() {
            let made = Command::new("mkfifo")
                .arg("-m")
                .arg("666")
                .arg(&ready)
                .status();
            assert!(made.expect("mkfifo").success());
        }
        dir
    }

    fn before(&mut self, program: Program) {
        if matches!(program, Program::Hog { own_limit: false }) {
            let dir = cgroup_dir(&self.cgroup);
            fs::write(dir.join("memory.max"), BUSY_NODE_LIMIT.to_string()).expect("memory.max");
            fs::write(dir.join("memory.swap.max"), "0").expect("memory.swap.max");
        }
    }

    fn after(&mut self, run_id: &str, program: Program) {
        if program.opens_ready() {
            let ready = self.base.join("sync").join(run_id).join("ready");
            // Blocks until the job opens the pipe: the job is up.
            let said = fs::read_to_string(ready).expect("read the pipe");
            assert_eq!(said.trim(), "up");
        }
    }

    fn settled(&mut self, id: &RunHandle) {
        self.crun.borrow_mut().wait_ended(id);
    }

    fn unstarted(&mut self, spec: &RunSpec) -> RunHandle {
        let staged = self.crun.borrow_mut().stage(spec);
        staged.expect("stage")
    }

    fn spoil(&mut self, spec: &mut RunSpec) {
        fs::remove_dir(&spec.scratch).expect("remove the scratch directory");
    }

    fn config(&mut self) -> Config {
        let scratch = self.made(&["scratch"], 0o711);
        let state = self.made(&["state"], 0o700);
        let sync = self.made(&["sync"], 0o711);
        Config {
            capacity: Capacity {
                cpu_millis: 4000,
                memory_bytes: 8 << 30,
                disk_bytes: 10 << 30,
                gpus: [0, 1].into(),
            },
            allow_host_jobs: false,
            mounts: MountRoots::new(&[("sync".to_owned(), sync)]).expect("mount roots"),
            scratch: ScratchRoot::open_with(&scratch, DirLayout::Searchable).expect("scratch root"),
            files: None,
            state: StateRoot::open(&state).expect("state root"),
            stop_grace: runtime_suite::LONG,
            bundle: None,
            quota: None,
        }
    }
}

impl Drop for CrunSubject {
    fn drop(&mut self) {
        remove_cgroup_tree(&cgroup_dir(&self.cgroup));
        if let Some((own, _)) = self.cgroup.rsplit_once('/') {
            let _ = fs::remove_dir(cgroup_dir(own));
        }
        let _ = fs::remove_dir_all(&self.base);
    }
}
