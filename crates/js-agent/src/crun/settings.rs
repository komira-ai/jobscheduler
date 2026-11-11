//! What a [`CrunRuntime`](super::CrunRuntime) is opened with ([`CrunSettings`], its
//! [`CgroupSettings`], [`HostPorts`] and [`Hooks`]) and why it could not be opened ([`CrunError`]).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::PortRange;
use js_contain::oci::{OciConfig, OciError};

use super::{
    CALL_LIMIT, EgressPort, GpuSettings, KILL_WAIT, LedgerError, PULL_LIMIT, hand_over_no_follow,
};
use crate::cli_runtime::Chown;
use crate::host_ports::{Kernel, Listen};
use crate::lifecycle::FilesRoot;

/// Where the agent's cgroups are.
#[derive(Debug, Clone)]
pub struct CgroupSettings {
    /// The cgroup version 2 mount.
    pub mount: PathBuf,
    /// The agent's delegated cgroup, relative to the mount, starting with `/`.
    pub root: String,
    /// The cgroup the leases' cgroups are made in, in the same form.
    pub actions: String,
}

/// What a [`CrunRuntime`] is opened with.
#[derive(Debug, Clone)]
pub struct CrunSettings {
    /// The backend's configuration: the `crun` program, the agent's own binary for the
    /// helper modes and the job user, which must be set (the agent is root).
    pub oci: OciConfig,
    /// The directory of the backend's image store and leases.
    pub scratch: PathBuf,
    /// The cgroups.
    pub cgroups: CgroupSettings,
    /// The id the agent registered under: the value of the agent label of every container.
    pub host_id: String,
    /// The threads that pull images.
    pub pull_threads: usize,
    /// The root of the runs' files, when the agent has one.
    pub files: Option<FilesRoot>,
    /// The host's egress, which wires each container's network: none in mode none, where a
    /// job has loopback only ([`start_egress`]).
    pub egress: Option<Arc<dyn EgressPort>>,
    /// The GPUs a run is given, when the agent passes them (`--gpu-device`).
    pub gpus: Option<GpuSettings>,
    /// The most bytes the unpacked images may take (`--image-budget-gib`); none for no budget.
    pub image_budget: Option<u64>,
    /// Whether a host job (`isolation = host`) is launched (`--allow-host-jobs`); without it
    /// one is refused before anything is made.
    pub host_jobs: bool,
    /// The host ports a job may reach, and what makes their listeners.
    pub host_ports: HostPorts,
}

/// The host ports of a [`CrunRuntime`]: which a job may name (`--host-port`), and what makes
/// the listeners in a job's network namespace ([`Kernel`] on a node).
#[derive(Debug, Clone)]
pub struct HostPorts {
    /// The ports of the host's loopback a job may reach; a start that names another is
    /// refused (`Runtime::host_ports`). None by default.
    pub allowed: Vec<PortRange>,
    /// What makes the listeners.
    pub listen: Arc<dyn Listen>,
}

impl Default for HostPorts {
    /// None allowed, listened for by the [`Kernel`].
    fn default() -> Self {
        Self {
            allowed: Vec::new(),
            listen: Arc::new(Kernel),
        }
    }
}

/// What a runtime does that a test replaces: giving a file away and reading the clock.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// Gives a path to a user and group.
    pub chown: Chown,
    /// Milliseconds since the Unix epoch.
    pub now_ms: fn() -> u64,
    /// How long a run is waited for after its cgroup was killed ([`KILL_WAIT`]).
    pub kill_wait: Duration,
    /// How long a pull and unpack of one image may take ([`PULL_LIMIT`]).
    pub pull_limit: Duration,
    /// How long a `crun kill` may take before it is given up ([`CALL_LIMIT`]).
    pub call_limit: Duration,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            chown: hand_over_no_follow,
            now_ms: system_now_ms,
            kill_wait: KILL_WAIT,
            pull_limit: PULL_LIMIT,
            call_limit: CALL_LIMIT,
        }
    }
}

pub(super) fn system_now_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    let ms = since.map_or(0, |since| since.as_millis());
    u64::try_from(ms).unwrap_or(u64::MAX)
}

/// Why a [`CrunRuntime`] could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum CrunError {
    /// The settings name no job user: the agent is root and a container runs as another.
    #[error("the crun runtime needs the job user the containers run as")]
    NoJobUser,
    /// The cgroup the leases are made in is not named `actions`, the name the post-job checks
    /// know the leases' group by: a job's leftover process under another name would be
    /// judged the host's.
    #[error("the cgroup of the leases must be named `actions`, not {actions}")]
    ActionsName {
        /// The cgroup given.
        actions: String,
    },
    /// The backend cannot start on this node.
    #[error(transparent)]
    Oci(#[from] OciError),
    /// A file of the agent's cgroup could not be given to the job user.
    #[error("give {path} to the job user: {why}")]
    Delegation {
        /// The file.
        path: PathBuf,
        /// The reason.
        why: io::Error,
    },
    /// The cgroup of the helper containers could not be made or given to the job user.
    #[error("the cgroup of the helper containers {path}: {why}")]
    Helpers {
        /// The cgroup.
        path: PathBuf,
        /// The reason.
        why: io::Error,
    },
    /// The directory of the image configs could not be made.
    #[error("the directory of the image configs {path}: {why}")]
    Configs {
        /// The directory.
        path: PathBuf,
        /// The reason.
        why: io::Error,
    },
    /// The runtime of the calls, or the threads that pull images, could not be made (no
    /// thread, or no descriptor, or no pull thread asked for).
    #[error("the threads of the crun runtime: {0}")]
    Threads(#[from] io::Error),
    /// The directory of the leases' labels could not be made or listed.
    #[error("the directory of the leases' labels {0}")]
    Ledger(#[from] LedgerError),
}
