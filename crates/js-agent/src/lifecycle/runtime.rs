//! The container runtime the lifecycle drives: what a backend (Docker or Podman, see
//! [`CliRuntime`](crate::cli_runtime::CliRuntime)) must do, and the plain data that
//! crosses the seam.
//!
//! The trait is small on purpose. Every call is blocking and idempotent where it can be
//! (`stop` and `remove` of a container that is already gone succeed), and a call that
//! fails says so in a [`RuntimeError`] whose text stays on the host: it is logged and
//! kept in a finding, and never put into a message to the server.
//!
//! The calls run on the lifecycle's own worker thread, never on the session's heartbeat
//! loop (see the [module](super) documentation). A call may take as long as the runtime
//! needs: the grace of a `stop`, or, for a runtime that has not [`Runtime::prepare`]d its
//! images, a pull inside `create_and_start`. It delays the lifecycle's next step and
//! nothing else. A runtime that pulls in the background ([`Prefetcher`](super::Prefetcher))
//! answers `prepare` at once and never makes the worker wait for a pull.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use js_agent_wire::{Isolation, LogTail};

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
    /// The booked bytes of scratch disk. A container without it (made by an earlier agent)
    /// is read as booking none.
    pub const DISK_BYTES: &str = "jobscheduler.dev/disk-bytes";
    /// The booked GPU indices, comma separated; empty for none.
    pub const GPUS: &str = "jobscheduler.dev/gpus";
    /// The wall-time limit in milliseconds.
    pub const WALL_MS: &str = "jobscheduler.dev/wall-ms";
    /// `host` for a job that may change the host, `contained` for the rest. A container
    /// without it (made by an earlier agent) is a contained job's. An agent that finds a
    /// `host` one after a restart taints the host.
    pub const ISOLATION: &str = "jobscheduler.dev/isolation";
    /// The slot the run holds (`slot-uid` of the module documentation of the lifecycle),
    /// as a number; absent on a container an earlier agent made.
    pub const SLOT: &str = "jobscheduler.dev/slot";
    /// The id the agent registered under. A runtime puts it on every container it makes
    /// and lists only the containers that carry it, so two agents sharing one runtime
    /// never adopt each other's jobs.
    pub const AGENT: &str = "jobscheduler.dev/agent";
    /// When a runtime made the container, in milliseconds since the Unix epoch; a
    /// runtime that can read it back reports a restarted container's age from it.
    pub const CREATED_MS: &str = "jobscheduler.dev/created-ms";
}

/// A container's id as the runtime names it. Opaque to the agent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunHandle(pub String);

/// A mount whose host path the agent has resolved against one of its own roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMount {
    /// The canonical host path, below the root the mount names, as it was when the agent
    /// resolved it. A backend must not trust it to be the same path at the moment it binds.
    /// A container daemon binds by path and cannot use a descriptor the agent opened, so a
    /// backend of that kind reads back where the daemon bound each mount and refuses the
    /// container when it differs; the time between the resolution and the bind stays open
    /// (see [`MountRoots`](super::MountRoots)).
    pub host: PathBuf,
    /// The path inside the container: `/mnt/<root name>/<rel>`.
    pub target: String,
    /// Whether the job sees it read-only.
    pub ro: bool,
}

/// What a run executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A container: an image and the command line after it.
    Container {
        /// The image.
        image: String,
        /// The command line after the image.
        args: Vec<String>,
    },
}

/// Everything a runtime needs to create and start one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// The run's id; also in [`RunSpec::labels`].
    pub run_id: String,
    /// What the run executes.
    pub body: Body,
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
    /// The directory of the run's files (`0555`, its files `0444`), mounted read-only at
    /// `/run/js-files`; none for a run without files.
    pub files: Option<PathBuf>,
    /// The slot the run holds: its container user and the owner of its scratch directory are
    /// the slot's ids ([`Settings::slot_ids`](crate::cli_runtime::Settings::slot_ids)).
    pub slot: u32,
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
    /// It runs, or it is created and about to: the agent polls only a container it has
    /// started, so a backend reports a created one as running. (One that was created and
    /// never started, found after a restart, is no job: see
    /// [`Runtime::list_labelled`].)
    Running,
    /// It has exited.
    Exited(Exit),
    /// The runtime no longer has it: it was removed from outside, or the host lost it, or
    /// it holds only what the runtime could not remove and no exit status to trust. The
    /// job ends as failed (or as stopped or timed out, when the agent was ending it).
    Gone,
}

/// A labelled container found on the host, for re-adoption after a restart. Whether it
/// still runs is found by polling it, like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The container.
    pub id: RunHandle,
    /// Its labels.
    pub labels: BTreeMap<String, String>,
    /// How long it has been running, in milliseconds, as the runtime reports it; counted
    /// against the job's wall-time limit.
    pub running_ms: u64,
    /// It was created and never started (Docker: `created`, Podman: `configured`): no job
    /// ever ran in it, whatever its labels say, so it is never adopted. The lifecycle
    /// evicts it like any container it cannot adopt.
    pub never_started: bool,
}

/// A runtime call that failed. The text is the runtime's own and stays on the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RuntimeError(pub String, bool);

impl RuntimeError {
    /// A failure of the call.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into(), false)
    }

    /// A start that failed because the agent cannot see the host: the container started,
    /// and the process the runtime names for it is not in the agent's `/proc`, so the
    /// post-job checks would read a host that is not the one the job runs on. The backend
    /// has removed the container; the lifecycle taints the host as unchecked.
    pub fn blind(text: impl Into<String>) -> Self {
        Self(text.into(), true)
    }

    /// Whether the failure is that the agent cannot see the host ([`RuntimeError::blind`]).
    pub fn is_blind(&self) -> bool {
        self.1
    }
}

/// Names one fetch of an image, so that every run that waited on it hears how it ended
/// (see [`Runtime::prepare`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(pub u64);

/// Whether the image of a run is on the host, as [`Runtime::prepare`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// The image is here: the container can be created without a pull.
    Ready,
    /// The image is being fetched by the fetch the ticket names; ask again at the next
    /// step, naming it.
    Pending(Ticket),
    /// The image could not be had. The reason stays on the host.
    Failed(RuntimeError),
}

/// What the lifecycle needs of a container runtime.
pub trait Runtime {
    /// Begins getting `image` onto the host when it is not there, and says where that
    /// stands. It never waits for a pull: a run whose image is [`Prepared::Pending`] is held
    /// as `starting` (and books its slot) without the lifecycle's worker being busy, so
    /// other runs go on starting, running and ending beside it, and the job's wall-time
    /// limit and the runner's [`CALL_LIMIT`](crate::cli_runtime::CALL_LIMIT) do not count
    /// the pull: both begin when the image is [`Prepared::Ready`] and the container is
    /// created.
    ///
    /// The lifecycle calls it with `waited: None` when a start is admitted and again at
    /// every step while the answer is `Pending`, naming the ticket it was given. Calls for
    /// one image while it is being fetched do not start a second fetch. A failure is
    /// reported to every call that names the ticket of the fetch that failed, so every run
    /// that waited on it ends; a call that names no ticket (a new start) or another one
    /// starts a new fetch.
    ///
    /// The default is a runtime that pulls inside [`Runtime::create_and_start`] (Docker and
    /// Podman, as `--container-pull` says): the image is always `Ready`. A runtime that
    /// prepares must answer `Ready` for a run that does not use the image store (an
    /// `isolation = host` run), and its fetch must have a hard total deadline of its own:
    /// nothing else ends a fetch that hangs, and every run that names the image waits on it.
    fn prepare(&mut self, _image: &str, _waited: Option<Ticket>) -> Prepared {
        Prepared::Ready
    }

    /// Creates and starts the container of `spec`. A call that fails leaves no
    /// container behind: the backend rolls back what it created. A run id that already has a
    /// container (running, exited or not yet removed) is refused, and that container is left
    /// as it is.
    ///
    /// # Errors
    ///
    /// The image cannot be had, the container cannot be created or it cannot start.
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError>;

    /// Whether the container runs, has exited or is gone, without waiting. A backend maps
    /// the runtime's "no such container" to [`Status::Gone`], not to an error: an error
    /// means the runtime could not say, and the job is polled again.
    ///
    /// # Errors
    ///
    /// The runtime cannot say.
    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError>;

    /// Asks the container to stop, and kills it when it has not exited after `grace`.
    ///
    /// # Errors
    ///
    /// The runtime cannot stop it.
    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError>;

    /// Removes the container, running or not. Removing one that is gone succeeds.
    ///
    /// # Errors
    ///
    /// The runtime cannot remove it.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError>;

    /// Every container carrying the run label ([`label::RUN`]) and this agent's label
    /// ([`label::AGENT`], the host id the backend puts on every container it makes), running
    /// or exited. The agent label scopes the list: two agents sharing one runtime never see
    /// each other's containers, and the agent may adopt, stop and remove every one it is
    /// given. A container that was created and never started holds no job, whatever its
    /// labels say; a backend lists it too, marked [`Found::never_started`], and removes
    /// nothing: listing is a read, so one container that cannot be removed never makes the
    /// whole list fail.
    ///
    /// # Errors
    ///
    /// The runtime cannot list.
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError>;

    /// The ids of the containers [`Runtime::list_labelled`] would list, without
    /// reading each one. The rescue at the agent's end uses it: it needs only the ids, and
    /// a container that cannot be read must still be stopped. A backend that can list
    /// without reading overrides it; the default lists in full.
    ///
    /// # Errors
    ///
    /// The runtime cannot list.
    fn list_ids(&mut self) -> Result<Vec<RunHandle>, RuntimeError> {
        let found = self.list_labelled()?;
        Ok(found.into_iter().map(|one| one.id).collect())
    }

    /// The end of what the exited container `id` wrote to its standard output and standard
    /// error, read before the container is removed. `None` is a backend that keeps no log.
    /// The lifecycle calls it once for each job that exited, and a failure never changes
    /// the job's result: the result goes out without a tail.
    ///
    /// # Errors
    ///
    /// The runtime cannot read the log.
    fn log_tail(&mut self, _id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        Ok(None)
    }

    /// Whether a container made here can use a GPU. A start that names a GPU where this
    /// is false is refused (`gpu-unsupported`) and creates nothing.
    fn supports_gpus(&self) -> bool {
        true
    }

    /// Whether the network a contained job gets is limited as the agent was told. Called
    /// before every contained start; a start whose check fails is refused
    /// (`egress-unchecked`) and creates nothing. A runtime with nothing to check passes.
    ///
    /// # Errors
    ///
    /// What is wrong with the network or the firewall; the text stays on the host.
    fn check_egress(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// The control group directory (an absolute path of the cgroup version 2 file system)
    /// of a container that runs, or `None` when the runtime cannot say: the container is
    /// not in the agent's `/proc`, its control group is not the unified one, or it lies
    /// outside the control group the agent was told holds its containers. The lifecycle
    /// asks when a job starts or is adopted and, for a run it started, again at every
    /// usage window until the runtime can say (`Lifecycle::find_cgroups`); a lease with no
    /// control group is not sampled. A runtime with no control groups keeps the default.
    fn cgroup(&mut self, _id: &RunHandle) -> Option<PathBuf> {
        None
    }
}
