//! One test's runtime: a scratch directory, a cgroup under the unit's delegated one, and
//! an [`Oci`] over the real `crun`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use js_contain::delegate::{self, Delegation, PROC_SELF_CGROUP};
use js_contain::oci::image::ImageSource;
use js_contain::oci::{Oci, OciConfig};
use js_contain::subids::{DaemonIds, IdFiles, daemon_ids};

/// The cgroup v2 mount.
pub const MOUNT: &str = "/sys/fs/cgroup";

/// What the setup writes to `supervisor/memory.min`.
const SUPERVISOR_MEMORY_MIN: u64 = 256 << 20;

/// What the setup writes to `actions/memory.max`: room for every test here, and below a
/// hosted runner's memory.
const ACTIONS_MEMORY_MAX: u64 = 4 << 30;

/// The unit's cgroup, set up by the code a daemon runs, once per test process. Each test
/// target runs in a unit of its own (`cargo xtask crun-tests`).
pub fn delegation() -> &'static Delegation {
    static DELEGATION: OnceLock<Delegation> = OnceLock::new();
    DELEGATION.get_or_init(|| {
        let own = std::fs::read_to_string(PROC_SELF_CGROUP).expect("read /proc/self/cgroup");
        delegate::delegate(
            Path::new(MOUNT),
            &own,
            SUPERVISOR_MEMORY_MIN,
            Some(ACTIONS_MEMORY_MAX),
        )
        .unwrap_or_else(|e| {
            panic!("set up the delegated cgroup (run through cargo xtask crun-tests): {e}")
        })
    })
}

/// The `crun` the tests run: `JS_TEST_CRUN`, else the first `crun` on the `PATH`.
pub fn crun() -> PathBuf {
    if let Some(path) = std::env::var_os("JS_TEST_CRUN") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("crun"))
        .find(|candidate| candidate.is_file())
        .expect("no crun on the PATH (set JS_TEST_CRUN): run through cargo xtask crun-tests")
}

/// The daemon user's ids and subordinate ranges, from the system's files.
pub fn ids() -> DaemonIds {
    daemon_ids(&IdFiles::system()).expect("a subordinate id range for this user")
}

/// One test's scratch directory, cgroup and runtime.
pub struct Cell {
    /// The runtime, over the real `crun`.
    pub oci: Arc<Oci>,
    /// The scratch directory (under the system temporary directory, whose ancestors every
    /// id can search: the containers do not run as the test's user).
    pub scratch: PathBuf,
    /// The cgroup the cell's helpers and leases are made under, relative to the mount.
    pub cgroup: String,
}

impl Cell {
    /// A cell named `name` fetching images with `source`. The helper containers run the
    /// `js-helper` binary of this package, which answers the default helper modes.
    pub fn new(name: &str, source: Arc<dyn ImageSource>) -> Self {
        let uid = rustix::process::getuid().as_raw();
        let scratch = std::env::temp_dir().join(format!("js-contain-crun-{uid}-{name}"));
        if scratch.exists() {
            // A previous run's files belong to subordinate ids: only a helper removes them.
            let stale = Oci::new(&config(), source.clone(), &scratch, &parent(name));
            if let Ok(stale) = stale {
                let _ = stale.remove_dir(&scratch);
            }
        }
        std::fs::create_dir_all(&scratch).expect("the scratch directory");
        let cgroup = parent(name);
        let dir = Path::new(MOUNT).join(cgroup.trim_start_matches('/'));
        if dir.exists() {
            std::fs::remove_dir(&dir).expect("remove a stale test cgroup");
        }
        std::fs::create_dir(&dir).expect("create the test cgroup");
        std::fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        let oci = Oci::new(&config(), source, &scratch, &cgroup).expect("the runtime starts");
        Self {
            oci,
            scratch,
            cgroup,
        }
    }

    /// The directory of the image with `digest` (`sha256:<hex>`) in the store.
    pub fn image_dir(&self, digest: &str) -> PathBuf {
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.scratch.join("oci-images").join(hex)
    }

    /// The names in the image store.
    pub fn store(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.scratch.join("oci-images"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let dir = Path::new(MOUNT).join(self.cgroup.trim_start_matches('/'));
            let _ = std::fs::remove_dir(dir);
            // What the cell left is owned by subordinate ids; a helper removes it.
            let _ = self.oci.remove_dir(&self.scratch);
        }
    }
}

/// The cgroup of the cell `name`, under the delegated one.
fn parent(name: &str) -> String {
    format!("{}/{name}", delegation().actions)
}

/// The runtime's configuration: the real `crun`, and the `js-helper` binary as the
/// daemon's own.
fn config() -> OciConfig {
    OciConfig {
        runtime: crun(),
        daemon: PathBuf::from(env!("CARGO_BIN_EXE_js-helper")),
        ..OciConfig::default()
    }
}
