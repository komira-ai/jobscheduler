//! A job's life on the host: from the `Start` that names it, through its container, to the
//! result the server acknowledges and the slot that frees (design 5.4 and 5.10).
//!
//! [`Lifecycle`] is a state machine over three seams: a [`ContainerRuntime`], a
//! [`CheckHook`] and a [`Clock`]. It consumes the wire types in memory only (it reads a
//! [`HeartbeatReply`](js_agent_wire::HeartbeatReply) and offers the
//! [`HeldRun`](js_agent_wire::HeldRun)s and [`RunResult`](js_agent_wire::RunResult)s of
//! the next heartbeat), and it reads no clock but the one it is given and sleeps never.
//!
//! # What the agent guarantees per job
//!
//! - **A start runs at most once.** It is idempotent on the run id: a start for a run the
//!   agent holds changes nothing, and so is one for a run it finished or was told to stop
//!   (the agent remembers the last [`RETIRED_CAP`] run ids). A start that reaches the
//!   agent later than its `valid_for_ms` allows (the agent's clock, measured from the
//!   moment the reply arrived) is dropped and nothing is sent.
//! - **A start that must be refused creates nothing** and is answered with a result of
//!   outcome `failed_to_start` and the refusal code as its detail, held like any other
//!   result: a host job on an agent without `--allow-host-jobs`
//!   (`host-jobs-not-allowed`), a job that names a GPU where the runtime's containers
//!   cannot use one (`gpu-unsupported`), a job larger than the machine or naming a GPU index it
//!   does not offer (`limits-exceed-shape`), one that does not fit what is free or
//!   names a booked GPU (`no-capacity`), and a mount that names no root, does not exist
//!   or leaves its root (`invalid-mount`).
//! - **A job is confined to its limits.** Its wall time is counted on the agent's clock
//!   from the moment its container started, and a job past it is stopped and reported
//!   `timed_out`.
//! - **A result is sent as soon as the container has exited**, before any cleanup, and
//!   it stays in the heartbeat until the server acknowledges it. An acknowledgement of a
//!   kind this version does not know keeps it.
//! - **Cleanup runs in one order**: the container, the scratch directory, the post-job
//!   checks, and only then the slot is freed. A step that fails stops the order there and
//!   is tried again at the next [`Lifecycle::step`]; the slot stays held and a
//!   [`Finding`] is recorded. A check that reports a finding blocks the slot for good (the
//!   taint of design 5.12 is the next change's).
//! - **A restart loses no job.** [`Lifecycle::adopt`] finds the containers the agent
//!   labelled, before the first heartbeat of the new process, and lists them first, so the
//!   server never sees a gap that would make it run the work twice.
//!
//! # Not wired yet
//!
//! Nothing here touches the network. The change that wires it will feed
//! [`Lifecycle::apply`] each heartbeat reply, call [`Lifecycle::step`] on each tick,
//! put [`Lifecycle::held`] and [`Lifecycle::results`] in the next heartbeat and a
//! capacity change from [`Lifecycle::free`] into the session, and build the [`Config`]
//! from the flags ([`Local`]) and the hello's shape ([`Capacity::of`]).

mod machine;
mod mounts;
mod runtime;
mod scratch;

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::cli::Cli;
use crate::cli_runtime::Settings;

pub use machine::{Lifecycle, RETIRED_CAP};
pub use mounts::{MountError, MountRoots};
pub use runtime::{
    ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, ResolvedMount, RuntimeError, Status,
    label,
};
pub use scratch::ScratchRoot;

/// The `detail` of a `failed_to_start` result when the cause is not a wire refusal code.
pub mod detail {
    /// The scratch directory could not be made.
    pub const SCRATCH_FAILED: &str = "scratch-failed";
    /// The runtime could not create or start the container.
    pub const RUNTIME_FAILED: &str = "runtime-failed";
    /// The job names a GPU and the runtime's containers cannot use one (a container on a
    /// Mac cannot reach the Apple GPU). Nothing is created.
    pub const GPU_UNSUPPORTED: &str = "gpu-unsupported";
}

/// The codes of a [`Finding`].
pub mod code {
    /// The runtime could not tell whether a container has exited.
    pub const POLL_FAILED: &str = "poll-failed";
    /// The runtime could not stop a container.
    pub const STOP_FAILED: &str = "stop-failed";
    /// The runtime could not start a container.
    pub const START_FAILED: &str = "start-failed";
    /// The runtime could not remove a container.
    pub const CONTAINER_REMOVE_FAILED: &str = "container-remove-failed";
    /// The scratch directory could not be made or removed.
    pub const SCRATCH_FAILED: &str = "scratch-failed";
    /// A post-job check found something.
    pub const CHECK_FAILED: &str = "check-failed";
    /// A labelled container could not be accounted for after a restart.
    pub const UNADOPTABLE: &str = "unadoptable-container";
}

/// A monotonic clock in milliseconds, from an origin of its own. The lifecycle only
/// subtracts two readings, so the origin does not matter and a change of the wall clock
/// cannot make a job run long or die early.
pub trait Clock {
    /// Milliseconds since the clock's origin.
    fn now_ms(&self) -> u64;
}

/// Runs after every job, after cleanup and before the slot is freed (design 5.12). The
/// next change puts the integrity checks here.
pub trait CheckHook {
    /// Checks the host after the run `run_id`.
    ///
    /// # Errors
    ///
    /// The text of what was found, or why the check could not run. The slot is not freed.
    fn run(&mut self, run_id: &str) -> Result<(), String>;
}

/// The hook with no checks: every job passes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChecks;

impl CheckHook for NoChecks {
    fn run(&mut self, _run_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Something the agent keeps because it needs a person or a later step: a cleanup that
/// failed, a check that found something, a container it could not account for. The text
/// is local; it is never put into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The run it is about, when it is about one.
    pub run_id: Option<String>,
    /// One of [`code`].
    pub code: &'static str,
    /// What the runtime, the file system or the check said.
    pub message: String,
}

/// Why a start was not acted on, when that is worth a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// It reached the agent after its validity window.
    Stale,
    /// The run was finished, or stopped, earlier.
    Retired,
    /// The agent already holds as many runs as a heartbeat can list, so it took on no
    /// more; the server may send the start again once results are acknowledged.
    Full,
}

/// What [`Lifecycle`] did, in order. The caller logs these; a test reads them as the
/// record of the order of things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A labelled container was taken over after a restart.
    Adopted {
        /// The run.
        run_id: String,
    },
    /// The container started.
    Started {
        /// The run.
        run_id: String,
    },
    /// The start was refused and nothing was created; a `failed_to_start` result is held.
    Refused {
        /// The run.
        run_id: String,
        /// The wire refusal code.
        code: &'static str,
        /// A local explanation.
        why: String,
    },
    /// The start was not acted on and nothing is sent.
    Skipped {
        /// The run.
        run_id: String,
        /// Why.
        why: Skip,
    },
    /// A command of the reply failed validation and was not acted on.
    Invalid {
        /// Its place in the reply.
        index: usize,
        /// The wire refusal code.
        code: &'static str,
    },
    /// The container exited and its result is held for the next heartbeat.
    ResultReady {
        /// The run.
        run_id: String,
    },
    /// The container is gone.
    ContainerRemoved {
        /// The run.
        run_id: String,
    },
    /// The scratch directory is gone.
    ScratchRemoved {
        /// The run.
        run_id: String,
    },
    /// The post-job checks passed.
    ChecksPassed {
        /// The run.
        run_id: String,
    },
    /// The slot is free again.
    SlotFreed {
        /// The run.
        run_id: String,
    },
    /// Something to look at; see [`Finding`]. Reported once for each code of a run.
    Finding(Finding),
}

/// What a host offers jobs: the shape of its hello and the GPU devices it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capacity {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// The GPU device indices on offer.
    pub gpus: BTreeSet<u32>,
}

impl Capacity {
    /// The capacity a hello offers.
    pub fn of(hello: &Hello) -> Self {
        Self {
            cpu_millis: hello.shape.cpu_millis,
            memory_bytes: hello.shape.memory_bytes,
            gpus: hello.gpu_devices.iter().map(|gpu| gpu.index).collect(),
        }
    }
}

/// How the lifecycle is set up.
#[derive(Debug, Clone)]
pub struct Config {
    /// What jobs may use in all.
    pub capacity: Capacity,
    /// Whether a start with `isolation = host` may run here.
    pub allow_host_jobs: bool,
    /// The roots a mount may name.
    pub mounts: MountRoots,
    /// Where the runs' scratch directories go.
    pub scratch: ScratchRoot,
    /// How long a container gets to stop on its own before it is killed.
    pub stop_grace: Duration,
}

/// The flags that configure jobs, checked.
#[derive(Debug, Clone)]
pub struct Local {
    /// `--allow-host-jobs`.
    pub allow_host_jobs: bool,
    /// The `--mount-root` flags, canonicalised.
    pub mounts: MountRoots,
    /// `--scratch-root`, when given.
    pub scratch: Option<ScratchRoot>,
    /// `--container-bin`, `--container-network` and `--container-user`, when containers
    /// are enabled by `--container-bin`.
    pub container: Option<Settings>,
}

impl Local {
    /// Checks the job flags of `cli`: every mount root exists and is canonicalised, the
    /// scratch root is private to the user running the agent, the container runtime is
    /// an absolute path to an executable file.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_cli(cli: &Cli) -> Result<Self, String> {
        Ok(Self {
            allow_host_jobs: cli.allow_host_jobs,
            mounts: MountRoots::new(&cli.mount_roots)?,
            scratch: cli
                .scratch_root
                .as_deref()
                .map(ScratchRoot::open)
                .transpose()?,
            container: Settings::from_flags(
                cli.container_bin.as_deref(),
                cli.container_network.as_deref(),
                cli.container_user.as_deref(),
            )
            .map_err(|why| why.to_string())?,
        })
    }
}
