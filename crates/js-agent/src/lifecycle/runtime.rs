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

use js_agent_wire::{
    HostPort, Isolation, LogReport, LogTail, NativeSpec, NetworkCeiling, NetworkLevel, PortRange,
    refusal,
};
use js_types::{JobNetwork, PortMap};

use crate::logs::Sink;

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
    /// `job` on a container a job start made (its run id is the job id and its lease id the
    /// allocation id), so a restarted agent lists it under the job keying again; absent on
    /// a container of a run start and on one an earlier agent made.
    pub const KEYING: &str = "jobscheduler.dev/keying";
    /// The host ports of a run the `crun` runtime offers them to (`RunSpec::host_ports`), each
    /// `HOST` when the job port is the host port and `HOST:JOB` when it is not, comma
    /// separated in the start's order, so a restarted agent offers them again; absent on a
    /// run with none and on a container an earlier agent made.
    pub const HOST_PORTS: &str = "jobscheduler.dev/host-ports";
    /// The id the agent registered under. A runtime puts it on every container it makes
    /// and lists only the containers that carry it, so two agents sharing one runtime
    /// never adopt each other's jobs.
    pub const AGENT: &str = "jobscheduler.dev/agent";
    /// When a runtime made the container, in milliseconds since the Unix epoch; a
    /// runtime that can read it back reports a restarted container's age from it.
    pub const CREATED_MS: &str = "jobscheduler.dev/created-ms";
    /// [`VM_KIND`] on the run of a VM start ([`js_agent_wire::StartKind::Vm`]): the
    /// runtime runs it in a fresh VM of its own ([`crate::vm`]). Absent on every other run.
    pub const KIND: &str = "jobscheduler.dev/kind";
    /// The value of [`KIND`] on the run of a VM start.
    pub const VM_KIND: &str = "vm";
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
    /// A program run on the host as a user of its own, under a sandbox profile (macOS only):
    /// the body of a native start as the wire carries it.
    Native(NativeSpec),
}

/// Everything a runtime needs to create and start one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// The run's id; also in [`RunSpec::labels`].
    pub run_id: String,
    /// What the run executes.
    pub body: Body,
    /// The environment a job start names, by name in order, which the runtime sets in the
    /// container over the image's own variables (a name in both takes this value), and the
    /// native runtime over the environment it builds from the body's own
    /// ([`NativeSpec::env`]). Empty for a run start. A runtime checks it before it creates anything
    /// ([`check_run_env`]), and one that cannot carry it refuses a run that has one
    /// ([`Runtime::check_env`]).
    pub env: Vec<(String, String)>,
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// The most processes, when bounded.
    pub pids: Option<u64>,
    /// The GPU device indices the job may use.
    pub gpu_indices: Vec<u32>,
    /// The mounts, already resolved. For a native body, its `mount` inputs, resolved
    /// read-only below their roots, each with the input's `dest` as its `target`.
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
    /// The network level the job was admitted at: never above the runtime's
    /// [`Runtime::network_ceiling`]. A backend that wires networks gives the container that
    /// level; `none` is loopback only.
    pub network: JobNetwork,
    /// The ports the job declared, each forwarded from the host's port to the job's (design
    /// 5.8) by a backend that wires networks; the admission let them through only when the
    /// runtime's ceiling lists their host ports and the level is wired (not `none`).
    pub ports: Vec<PortMap>,
    /// The ports of the host's loopback the job reaches at ports of its own loopback (the
    /// other direction from [`RunSpec::ports`]), at every network level: a backend that
    /// offers them listens on each job port in the job's network namespace and joins each
    /// connection to the host port. The admission let them through only when the runtime
    /// allows each host port ([`Runtime::host_ports`]).
    pub host_ports: Vec<HostPort>,
    /// The job's labels and the agent's own ([`label`]); the agent's win.
    pub labels: BTreeMap<String, String>,
    /// The darwin bundle a native run pins, `name[:tag]@sha256:<manifest digest>`: the
    /// runtime pins its trees for the run, may resolve `@bundle/<entrypoint>` through
    /// it, and unpins them when the run is removed. The agent has judged it already
    /// ([`Admission`](crate::bundle::Admission)); a runtime that holds no bundle store
    /// fails a start that has one.
    pub bundle: Option<String>,
    /// Where the run's output is uploaded while it runs: the checked sink of a job start
    /// ([`Sink::of_start`]), `None` for a run start and a start without one. A runtime that
    /// uploads no live logs ignores it.
    pub log_sink: Option<Sink>,
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
pub struct RuntimeError(pub String, Kind);

/// What kind of failure a [`RuntimeError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The call failed.
    Call,
    /// The agent cannot see the host ([`RuntimeError::blind`]).
    Blind,
    /// A removal that ended everything of the run and could not copy a declared output
    /// ([`RuntimeError::outputs`]).
    Outputs,
}

impl RuntimeError {
    /// A failure of the call.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into(), Kind::Call)
    }

    /// A start that failed because the agent cannot see the host: the container started,
    /// and the process the runtime names for it is not in the agent's `/proc`, so the
    /// post-job checks would read a host that is not the one the job runs on. The backend
    /// has removed the container; the lifecycle taints the host as unchecked.
    pub fn blind(text: impl Into<String>) -> Self {
        Self(text.into(), Kind::Blind)
    }

    /// Whether the failure is that the agent cannot see the host ([`RuntimeError::blind`]).
    pub fn is_blind(&self) -> bool {
        self.1 == Kind::Blind
    }

    /// A removal that did everything a removal does (the run's processes and its user are
    /// gone) but could not copy one or more of the run's declared outputs. Nothing of the
    /// run is left on the host, so the lifecycle reports it and does not taint the host.
    pub(crate) fn outputs(text: impl Into<String>) -> Self {
        Self(text.into(), Kind::Outputs)
    }

    /// Whether the failure is only that outputs were not copied ([`RuntimeError::outputs`]).
    pub(crate) fn is_outputs(&self) -> bool {
        self.1 == Kind::Outputs
    }
}

/// Checks the environment of a run before a runtime creates anything for it, with the
/// wire's one check ([`js_agent_wire::check_env`]): a run reaches a runtime by paths that
/// skip the wire's validation, and a name the agent reserves (the loader's, the agent's
/// own) must never reach a job's process. A name given twice is refused as well.
///
/// # Errors
///
/// The rule the environment breaks, naming `env` and never a name or a value.
pub fn check_run_env(env: &[(String, String)]) -> Result<(), RuntimeError> {
    let names: BTreeMap<String, String> = env.iter().cloned().collect();
    if names.len() != env.len() {
        return Err(RuntimeError::new("env names a variable twice"));
    }
    js_agent_wire::check_env("env", &names).map_err(|why| RuntimeError::new(why.to_string()))
}

/// Why a runtime will not run a native body: the wire code the start is answered with and
/// the text that stays on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRefusal {
    /// A refusal code of the wire ([`refusal`]).
    pub code: &'static str,
    /// What is wrong, for the log.
    pub why: String,
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

    /// What the capture saw of each stream of the exited container `id` (its length, and the
    /// bytes it could not keep), for a run whose output this runtime uploads; read beside
    /// [`Runtime::log_tail`], before the container is removed. It never waits for the upload.
    /// The default, a runtime that uploads nothing, has none.
    fn logs_report(&mut self, _id: &RunHandle) -> Option<LogReport> {
        None
    }

    /// Whether this runtime runs the native body `native`, asked before a native start
    /// creates anything. The default runs none, which is every runtime but the one a Mac
    /// agent has: a native start on a Linux host is refused `native-unsupported`.
    ///
    /// # Errors
    ///
    /// The code the start is refused with, and why.
    fn check_native(&self, _native: &NativeSpec) -> Result<(), NativeRefusal> {
        Err(NativeRefusal {
            code: refusal::NATIVE_UNSUPPORTED,
            why: "this agent runs no native jobs".to_owned(),
        })
    }

    /// Whether this runtime runs a VM start ([`js_agent_wire::StartKind::Vm`]) now, asked at
    /// admission before anything is created for one. The default runs none, which is every
    /// runtime but the VM one ([`crate::vm::WithVms`]): a VM start is refused
    /// [`refusal::VM_UNAVAILABLE`]. The VM runtime refuses one when both of its VM slots are
    /// held ([`refusal::NO_CAPACITY`]).
    ///
    /// # Errors
    ///
    /// The code the start is refused with, and why.
    fn check_vm(&self) -> Result<(), NativeRefusal> {
        Err(NativeRefusal {
            code: refusal::VM_UNAVAILABLE,
            why: "this agent runs no VMs".to_owned(),
        })
    }

    /// Whether this runtime carries a job start's environment (`env`, by name in order),
    /// asked at admission for every start, before anything is created and before
    /// [`Runtime::check_image`], so a start the runtime cannot give its environment is
    /// answered for that and not for another reason. The default carries it, which every
    /// runtime of this agent does (the container ones and the Mac's native one); a runtime
    /// that could not refuses a non-empty one (`env-unsupported`): a job must never run
    /// without the environment it names.
    ///
    /// # Errors
    ///
    /// The code the start is refused with, and why.
    fn check_env(&self, _env: &[(String, String)]) -> Result<(), NativeRefusal> {
        Ok(())
    }

    /// Whether this runtime runs a start named by its image (an `image`, with or without a
    /// `run`), asked before such a start creates anything. The default runs them, which is
    /// every container runtime. The runtime a Mac agent has runs one only with a `run` and a
    /// bundle store ([`Runtime::runs_image_runs`]); otherwise it refuses it
    /// (`image-unsupported-here`).
    ///
    /// # Errors
    ///
    /// The code the start is refused with, and why.
    fn check_image(&self) -> Result<(), NativeRefusal> {
        Ok(())
    }

    /// Whether this runtime runs a start named by its image with a `run`: the Mac runtime
    /// with a bundle store does, by the bundle the image resolves to, and runs the `run` as
    /// a native body. The default is a runtime that does not (a container runtime refuses a
    /// `run`, `run-unsupported`).
    fn runs_image_runs(&self) -> bool {
        false
    }

    /// Ends and removes whatever an earlier process left, which no list of the runtime's
    /// can say: the leases a native runtime journalled and never saw deleted. Called when a
    /// worker starts and by the rescue, before anything else; returns one line for each
    /// thing done or not done. A runtime whose leftovers are listed (the container one)
    /// has nothing to do.
    fn reclaim(&mut self) -> Vec<String> {
        Vec::new()
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

    /// The most network a contained job of this runtime may ask for (design 5.8): a start
    /// above it is refused (`network-above-ceiling`, `port-above-ceiling`,
    /// `ports-not-supported`) before anything is made, and never run with less. The default is
    /// `none` with no port: the container-engine runtimes (Docker, Podman) and the native one
    /// give a job no network of its own choosing and forward no port, so a job that asks for
    /// more through them is refused naming the ceiling `none`, and one that declares a port
    /// `ports-not-supported`. The `crun` runtime answers its egress's ceiling.
    fn network_ceiling(&self) -> NetworkCeiling {
        NetworkCeiling::default()
    }

    /// The ports of the host's loopback a job of this runtime may reach (`--host-port`,
    /// [`RunSpec::host_ports`]): a start that names any other is refused `input-not-allowed`
    /// before anything is made, and never run without it. The default is none: only the
    /// `crun` runtime offers host ports, and a start through any other that names one is
    /// refused, never run without the ports it named.
    fn host_ports(&self) -> Vec<PortRange> {
        Vec::new()
    }

    /// Why the run `id`, which this runtime adopted from an earlier process of the agent,
    /// could not be offered its host ports again ([`RunSpec::host_ports`]): the lifecycle
    /// stops it, and its result says `host-port-lost`. `None` for a run that lost none, and
    /// from a runtime that offers no host ports.
    fn host_port_lost(&self, _id: &RunHandle) -> Option<String> {
        None
    }

    /// Whether the network a native job would get is limited as the agent was told. Called
    /// before every native start, and only for a native start (a container start has
    /// [`Runtime::check_egress`]); a start whose check fails is refused (`egress-unchecked`)
    /// and creates nothing.
    ///
    /// The default fails closed: a runtime that does not say how it limits the network
    /// refuses a job that asks for it (`on`, the filtered network, or `open`) and passes one
    /// that does not. A runtime that takes native jobs and limits the network overrides it.
    ///
    /// The check is made at admission. The window from there to the helper's `run` (the
    /// slot, the user, the launch) is not checked again, and a run that is under way is not
    /// checked again after a reload of the packet filter.
    ///
    /// # Errors
    ///
    /// What is wrong with the limit; the text stays on the host.
    fn check_native_egress(&self, native: &NativeSpec) -> Result<(), RuntimeError> {
        match native.network {
            NetworkLevel::On | NetworkLevel::Open => Err(RuntimeError::new(
                "this runtime does not say how it limits the network of a native job",
            )),
            NetworkLevel::Off | NetworkLevel::Loopback => Ok(()),
        }
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

    /// The directory, directly below the scratch root, where this runtime keeps the writable
    /// state of the container of `run_id` that is not in the run's own scratch directory (a
    /// container's `/work` overlay and home), or `None` when it keeps none there. The
    /// lifecycle makes the directory empty beside the run's, puts it under the run's scratch
    /// quota, and removes it with the run's, so that one bound covers what the job writes
    /// everywhere. The runtime makes what goes in it and removes the directory with the
    /// container.
    fn lease_dir(&self, _run_id: &str) -> Option<PathBuf> {
        None
    }

    /// Whether every container this runtime starts is in a control group the agent can
    /// read ([`Runtime::cgroup`]). A runtime that says so and then cannot name a run's
    /// control group is blind to that run: the lifecycle taints the host as unchecked
    /// when it watches the run. The default is `false`: a runtime that keeps no control groups
    /// of its own is never blind for lack of one.
    fn places_in_cgroup(&self) -> bool {
        false
    }
}
