//! The container runtime the lifecycle drives: what a backend (Docker or Podman, in a
//! later change) must do, and the plain data that crosses the seam.
//!
//! The trait is small on purpose. Every call is blocking and idempotent where it can be
//! (`stop` and `remove` of a container that is already gone succeed), and a call that
//! fails says so in a [`RuntimeError`] whose text stays on the host: it is logged and
//! kept in a finding, and never put into a message to the server.
//!
//! The calls run on the lifecycle's own worker thread, never on the session's heartbeat
//! loop (see the [module](super) documentation), so a call may take as long as the runtime
//! needs: a pull inside `create_and_start`, the grace of a `stop`. It delays the lifecycle's
//! next step and nothing else.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use js_agent_wire::Isolation;

/// Container labels the agent writes, under the prefix the wire reserves, so a job's own
/// labels (which may not use the prefix) cannot forge them. They carry what a restarted
/// agent needs to account for a container it finds: whose it is and what it booked.
pub mod label {
    /// The run's id.
    pub const RUN: &str = "jobscheduler.dev/run-id";
    /// The lease the run belongs to.
    pub const LEASE: &str = "jobscheduler.dev/lease-id";
    /// The booked thousandths of a CPU core.
    pub const CPU_MILLIS: &str = "jobscheduler.dev/cpu-millis";
    /// The booked bytes of memory.
    pub const MEMORY_BYTES: &str = "jobscheduler.dev/memory-bytes";
    /// The booked GPU indices, comma separated; empty for none.
    pub const GPUS: &str = "jobscheduler.dev/gpus";
    /// The wall-time limit in milliseconds.
    pub const WALL_MS: &str = "jobscheduler.dev/wall-ms";
}

/// A container's id as the runtime names it. Opaque to the agent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContainerId(pub String);

/// A mount whose host path the agent has resolved against one of its own roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMount {
    /// The canonical host path, below the root the mount names, as it was when the agent
    /// resolved it. A backend must not trust it to be the same path at the moment it binds:
    /// it opens the path by file descriptor, beneath the root, and binds that (see
    /// [`MountRoots`](super::MountRoots)).
    pub host: PathBuf,
    /// The path inside the container: `/mnt/<root name>/<rel>`.
    pub target: String,
    /// Whether the job sees it read-only.
    pub ro: bool,
}

/// Everything a runtime needs to create and start one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The run's id; also in [`ContainerSpec::labels`].
    pub run_id: String,
    /// The image.
    pub image: String,
    /// The command line after the image.
    pub args: Vec<String>,
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// The most processes, when bounded.
    pub pids: Option<u64>,
    /// The GPU device indices the job may use.
    pub gpu_indices: Vec<u32>,
    /// The mounts, already resolved.
    pub mounts: Vec<ResolvedMount>,
    /// The run's scratch directory, mounted read-write.
    pub scratch: PathBuf,
    /// Container or host. The backend decides what a host job gets; the lifecycle only
    /// carries the field.
    pub isolation: Isolation,
    /// The job's labels and the agent's own ([`label`]); the agent's win.
    pub labels: BTreeMap<String, String>,
}

/// How a container ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The exit status.
    pub code: i32,
    /// Whether the kernel's memory controller killed it.
    pub oom_killed: bool,
}

/// What a poll found of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It runs, or it is created and about to: there is no state between a created
    /// container and a running one, so a backend reports a created container as running.
    Running,
    /// It has exited.
    Exited(Exit),
    /// The runtime no longer has it: it was removed from outside, or the host lost it. The
    /// job ends as failed (or as stopped or timed out, when the agent was ending it).
    Gone,
}

/// A labelled container found on the host, for re-adoption after a restart. Whether it
/// still runs is found by polling it, like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The container.
    pub id: ContainerId,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// How long it has been running, in milliseconds, as the runtime reports it; counted
    /// against the job's wall-time limit.
    pub running_ms: u64,
}

/// A runtime call that failed. The text is the runtime's own and stays on the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RuntimeError(pub String);

/// What the lifecycle needs of a container runtime.
pub trait ContainerRuntime {
    /// Creates and starts the container of `spec`. A call that fails leaves no
    /// container behind: the backend rolls back what it created.
    ///
    /// # Errors
    ///
    /// The image cannot be had, the container cannot be created or it cannot start.
    fn create_and_start(&mut self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError>;

    /// Whether the container runs, has exited or is gone, without waiting. A backend maps
    /// the runtime's "no such container" to [`Status::Gone`], not to an error: an error
    /// means the runtime could not say, and the job is polled again.
    ///
    /// # Errors
    ///
    /// The runtime cannot say.
    fn poll(&mut self, id: &ContainerId) -> Result<Status, RuntimeError>;

    /// Asks the container to stop, and kills it when it has not exited after `grace`.
    ///
    /// # Errors
    ///
    /// The runtime cannot stop it.
    fn stop(&mut self, id: &ContainerId, grace: Duration) -> Result<(), RuntimeError>;

    /// Removes the container, running or not. Removing one that is gone succeeds.
    ///
    /// # Errors
    ///
    /// The runtime cannot remove it.
    fn remove(&mut self, id: &ContainerId) -> Result<(), RuntimeError>;

    /// Every container carrying the agent's run label ([`label::RUN`]), running or
    /// exited.
    ///
    /// # Errors
    ///
    /// The runtime cannot list.
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError>;
}
