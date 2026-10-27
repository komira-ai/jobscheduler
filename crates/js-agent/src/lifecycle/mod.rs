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
//! - **A result says what happened.** It is sent as soon as the container has exited,
//!   before any cleanup, and it stays in the heartbeat until the server acknowledges it. An
//!   acknowledgement of a kind this version does not know keeps it. A container that
//!   exited before a stop reached it (a `Stop` applied between the exit and the next step,
//!   or a stop the runtime refused) is reported as it exited, `succeeded` with status 0
//!   included; `killed` and `timed_out` mean the agent's stop ended it, or that the
//!   container was gone when the agent looked.
//! - **A result with a failure code is a result.** A `failed_to_start` result with a
//!   refusal code or one of [`detail`] is terminal: the run did not start and will not
//!   be tried again here, and the server counts it like any other failure of the run.
//! - **Cleanup runs in one order**: the container, the scratch directory, the post-job
//!   checks, and only then the slot is freed. A step that fails stops the order there and
//!   is tried again at the next [`Lifecycle::step`]; the slot stays held and a
//!   [`Finding`] is recorded. A retry that succeeds does not free the slot by itself: the
//!   order goes on, the checks run after the cleanup that failed, and the slot is freed
//!   only if they ran clean and the marker is on disk (a job whose checks find something
//!   stays blocked).
//! - **A restart loses no running job.** [`Lifecycle::adopt`] finds the containers the
//!   agent labelled, before the first heartbeat of the new process, and lists them first,
//!   so the server never sees a gap that would make it run the work twice. A labelled
//!   container no job can stand for (bad or repeated labels, a booking outside the offer
//!   or beside one already taken over, a number of runs beyond what a heartbeat can
//!   list) is stopped and removed by its own id and recorded as a [`Finding`]. One that
//!   cannot be removed holds what its labels say it booked (the whole offer when they do
//!   not say) and is stopped and removed again at every step, so nothing is left running
//!   with nothing booked for it. Containers are taken over in the order of their age
//!   (those that have exited first, then the longest running), so an offer lowered since
//!   the last start evicts the newest jobs that no longer fit.
//!
//! - **A host that is not known to be clean is tainted** (design 5.12, see the [`taint`]
//!   module). A check that finds something or cannot run, a container or scratch
//!   directory that cannot be removed, a job that was allowed to change the host
//!   (`isolation = host`, tainted when it starts, before its container exists), a labelled
//!   container that no job can stand for and that may be a host job's (one labelled `host`,
//!   with no isolation label or with one the agent never writes: removed, and tainted
//!   first), and the record of a job's owed checks found when the agent starts, each write
//!   the taint marker under `--state-root`. The marker is written before the taint is
//!   reported: [`Lifecycle::pending_taint`] offers it for the heartbeat only once the
//!   marker is on disk, so a taint the server was told of is a taint a restart finds, and a
//!   taint on disk is reported at the first heartbeat of the process that finds it. (The
//!   run's own result is not held back by it: it is reported as soon as the container has
//!   exited, before its cleanup and checks.) A tainted agent answers every later `Start`
//!   `failed_to_start` with the code `host-tainted`, lets the jobs it runs finish, and
//!   offers the taint until the server acknowledges it. A marker that cannot be written
//!   keeps the agent tainted in memory (it refuses starts and frees no slot) and the taint
//!   is not reported until it is written; the marker is tried again at every step.
//! - **A check owed is not lost with the agent.** Before the first step of a job's cleanup
//!   the agent records, in the state root, that the job's checks are owed; it removes the
//!   record once they have run (and, when they found something, once the taint is on
//!   disk). If the agent stops in between, the record is found when the next process
//!   starts, and the host is tainted (`check-owed`): what the job left behind may be part
//!   of the baseline the new process took. A record that cannot be written holds the
//!   cleanup where it is, the container included; one that cannot be removed holds the
//!   slot. **A restart or a `SIGTERM` inside the cleanup window therefore taints the host
//!   at the next start**, and until a flow to clear a taint exists nothing ends it; that
//!   flow must remove the `owed-*` records as well as `taint.json`, or the next start
//!   taints the host again (see [`taint`]).
//! - **A `host-tainted` refusal is final for that `Start`, and the host's.** The run did
//!   not start on this host and will not; the server must not send it to this host again
//!   (see `refusal::HOST_TAINTED`), and must treat a `Start` refused with it as the host's
//!   taint even when the heartbeat carries none (the marker may be unwritable, and a taint
//!   that is not on disk is not reported), and must not count the refusal against the
//!   run's retries. `taint_ack` acknowledges the taint this process reported: an
//!   acknowledgement that arrives before the taint was reportable (not on disk) counts for
//!   nothing, and a restarted process reports the taint again. **The agent does not
//!   check which session an acknowledgement belongs to**: that the server answers
//!   `taint_ack` only to the session that carried the taint is the server's rule, and the
//!   agent takes any `taint_ack` of a reply it accepts once its taint is on disk.
//!
//! # What this change does not do
//!
//! - **A blocked run never leaves.** A check that finds something, or cannot run, leaves
//!   the run `Blocked`: its slot stays booked, it counts toward the runs the agent holds
//!   and is listed as `exited_unacked` for as long as the agent lives. Nothing clears a
//!   taint yet: the host is reimaged, which is the clear flow's to add.
//! - **Mount roots are read-write.** A root is whatever directory the operator names, and
//!   a job's `ro` flag is the message's. A root that is read-only for every job is a later
//!   flag. A job that holds a read-write mount can write in its root, so it can swap a
//!   directory of the root for a link while another job's mount is being set up. A
//!   container daemon binds by path and cannot use a descriptor the agent opened, so the
//!   backend reads back where each mount was bound and refuses a container whose mounts
//!   differ; the time between the agent's resolution and the daemon's bind stays open (see
//!   [`MountRoots`]).
//! - **Held and retired state is in memory.** After a restart an unacknowledged result
//!   whose container was removed is lost, and the agent forgets which run ids it was told
//!   to stop; a late `Start` for one of them runs. Containers that still exist are
//!   re-adopted (and a run that exited in the meantime is reported as its container
//!   says). The scheduler reruns a lease the agent no longer reports.
//! - **A `Stop` or `Cancel` of a run the agent does not hold retires the run id on
//!   purpose**, so that a `Start` that arrives after it never runs.
//! - **Cgroup emptiness is not checked.** A container is removed, and nothing reads its
//!   control group to see that no process of the job is left in it.
//! - **A user-namespace remapping engine is not supported.** The container user would be
//!   another uid on the host, and the check for a job's leftover processes compares the
//!   host's uid with `--container-user`.
//! - **Host jobs are not available.** `--allow-host-jobs` is refused at start until the
//!   scheduler records a host job's taint itself ([`Local::from_cli`]).
//!   [`Config::allow_host_jobs`] is the switch the lifecycle honours once that exists; it
//!   already taints the host when such a job starts.
//!
//! # The threading contract of the runtime seam
//!
//! [`ContainerRuntime`] calls block (an image pull, the grace of a stop, a slow daemon).
//! They are made by [`Lifecycle`] only. No worker thread exists yet: this is a
//! requirement on the change that wires the lifecycle to the session, and on the backend.
//! A [`Lifecycle`] must be driven from its own worker thread, never from the session's
//! heartbeat loop, so that a slow runtime call delays the next [`Lifecycle::step`] but
//! never a heartbeat, and a slow docker call cannot make the agent miss its heartbeat
//! deadline and fence itself. The worker owns the lifecycle (build it on that thread, so
//! the runtime needs no `Send` bound); the session loop hands it replies and takes
//! [`Lifecycle::held`], [`Lifecycle::results`] and [`Lifecycle::free`] as snapshots the
//! worker publishes after each call.
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
pub mod taint;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::{Hello, Taint};

use crate::FileReader;
use crate::cli::Cli;
use crate::cli_runtime::{Flags, Settings};

pub use machine::{Lifecycle, RETIRED_CAP};
pub use mounts::{MountError, MountRoots};
pub use runtime::{
    ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, ResolvedMount, RuntimeError, Status,
    label,
};
pub use scratch::ScratchRoot;
pub use taint::{MARKER_FILE, StateRoot};

/// The `detail` of a `failed_to_start` result when the cause is not a wire refusal code.
pub mod detail {
    /// The scratch directory could not be made.
    pub const SCRATCH_FAILED: &str = "scratch-failed";
    /// The runtime could not create or start the container.
    pub const RUNTIME_FAILED: &str = "runtime-failed";
    /// The job names a GPU and the runtime's containers cannot use one (a container on a
    /// Mac cannot reach the Apple GPU). Nothing is created.
    pub const GPU_UNSUPPORTED: &str = "gpu-unsupported";
    /// The job may change the host and the taint marker could not be written, so the job
    /// was not started.
    pub const TAINT_FAILED: &str = "taint-failed";
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
    /// A labelled container could not be accounted for after a restart.
    pub const UNADOPTABLE: &str = "unadoptable-container";
    /// A container that could not be accounted for could not be removed either, so it is
    /// still on the host, unbooked.
    pub const UNADOPTABLE_KEPT: &str = "unadoptable-container-kept";
    /// A started container's process is not in the agent's `/proc`: the agent does not see
    /// the host its jobs run on. The container was removed and the host is tainted as
    /// unchecked.
    pub const HOST_NOT_VISIBLE: &str = "host-not-visible";
    /// The taint marker could not be written. The host is tainted in memory all the same.
    pub const TAINT_WRITE_FAILED: &str = "taint-write-failed";
    /// The record that a job's checks are owed could not be written or removed. While it
    /// cannot be written the job's container is not removed; while it cannot be removed
    /// the job's slot is not freed.
    pub const OWED_FAILED: &str = "owed-record-failed";
    /// The records of owed checks in the state root could not be listed when the agent
    /// started, so the host is tainted ([`reason::CHECK_OWED`](super::taint::reason)).
    pub const OWED_UNREADABLE: &str = "owed-records-unreadable";
}

/// A monotonic clock in milliseconds, from an origin of its own. The lifecycle only
/// subtracts two readings, so the origin does not matter and a change of the wall clock
/// cannot make a job run long or die early.
pub trait Clock {
    /// Milliseconds since the clock's origin.
    fn now_ms(&self) -> u64;
}

/// A job whose container and scratch directory are gone, as the post-job checks see it.
#[derive(Debug, Clone, Copy)]
pub struct Finished<'a> {
    /// The run.
    pub run_id: &'a str,
    /// The containers of the agent's other jobs that are still on the host (running, or
    /// exited and not yet removed), and those that could not be removed. What they run and
    /// mount is not the finished job's, so a check must not count it against the job.
    pub live: &'a [ContainerId],
}

/// Runs after every job, after cleanup and before the slot is freed (design 5.12). The
/// integrity checks of [`crate::integrity`] are the hook of an agent that runs jobs.
pub trait CheckHook {
    /// Checks the host after the job `job`.
    ///
    /// # Errors
    ///
    /// What was found, or that the check could not run. The slot is not freed and the host
    /// is tainted.
    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding>;
}

/// The most bytes of detail a [`CheckFinding`] keeps.
pub const CHECK_DETAIL_MAX: usize = 256;

/// What a post-job check reports when the host is not known to be clean: a short code the
/// check owns, and a detail cut to [`CHECK_DETAIL_MAX`] bytes so that a check cannot fill
/// the agent's memory or its log with the output of a program. The code becomes the code
/// of the run's [`Finding`], so it is one finding for each code of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFinding {
    /// The run left something behind on the host.
    Residue {
        /// What kind, as a short code of the check.
        code: &'static str,
        /// What the check saw.
        detail: String,
    },
    /// The check could not run, so the host is not known to be clean.
    Unchecked {
        /// Which check, as a short code.
        code: &'static str,
        /// Why it could not run.
        detail: String,
    },
}

impl CheckFinding {
    /// A [`CheckFinding::Residue`] of `code`; `detail` is cut to [`CHECK_DETAIL_MAX`]
    /// bytes, at a character boundary.
    pub fn residue(code: &'static str, detail: &str) -> Self {
        Self::Residue {
            code,
            detail: bounded(detail).to_owned(),
        }
    }

    /// A [`CheckFinding::Unchecked`] of `code`; `detail` is cut to [`CHECK_DETAIL_MAX`]
    /// bytes, at a character boundary.
    pub fn unchecked(code: &'static str, detail: &str) -> Self {
        Self::Unchecked {
            code,
            detail: bounded(detail).to_owned(),
        }
    }

    /// The short code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Residue { code, .. } | Self::Unchecked { code, .. } => code,
        }
    }

    /// The detail, at most [`CHECK_DETAIL_MAX`] bytes: cut here too, so a finding built
    /// as a struct literal, which skips the constructors, is bounded where it is read.
    pub fn detail(&self) -> &str {
        match self {
            Self::Residue { detail, .. } | Self::Unchecked { detail, .. } => bounded(detail),
        }
    }
}

/// `text` cut to at most [`CHECK_DETAIL_MAX`] bytes, at a character boundary.
fn bounded(text: &str) -> &str {
    let end = (0..=CHECK_DETAIL_MAX.min(text.len()))
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0);
    &text[..end]
}

/// The hook with no checks: every job passes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChecks;

impl CheckHook for NoChecks {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
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
    /// One of [`code`], or the code of a [`CheckFinding`].
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
    /// The host is tainted from now on, and this is why (the first taint stays). It is
    /// pushed once the marker has been tried and before the findings that follow from the
    /// same cause; a failure to write the marker is a [`Finding`] after it.
    Tainted(Taint),
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
    /// Where the taint marker is kept.
    pub state: StateRoot,
    /// How long a container gets to stop on its own before it is killed.
    pub stop_grace: Duration,
}

/// The flags that configure jobs, checked.
#[derive(Debug, Clone)]
pub struct Local {
    /// The `--mount-root` flags, canonicalised.
    pub mounts: MountRoots,
    /// `--scratch-root`, when given.
    pub scratch: Option<ScratchRoot>,
    /// `--container-bin`, `--container-network` and `--container-user`, when containers
    /// are enabled by `--container-bin`.
    pub container: Option<Settings>,
    /// `--state-root`, when given; always given when containers are enabled.
    pub state: Option<StateRoot>,
    /// The `--watch-dir` flags, canonicalised, sorted, each once.
    pub watch: Vec<PathBuf>,
}

impl Local {
    /// Checks the job flags of `cli`: `--allow-host-jobs` is refused (the scheduler does
    /// not record a host job's taint yet), every mount root exists and is canonicalised,
    /// the scratch root and the state root are private to the user running the agent, the
    /// container runtime is an absolute path to an executable file, containers need a state
    /// root (the taint marker is kept there) that is not, and does not lie in, a mount root
    /// or the scratch root (a job with a read-write mount could delete the marker), and every watched directory is an absolute
    /// path to an existing directory that no mount root, scratch root or state root lies in
    /// or holds (those change with every job).
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_cli(cli: &Cli) -> Result<Self, String> {
        if cli.allow_host_jobs {
            return Err(
                "--allow-host-jobs: host jobs are not available yet: the scheduler \
                        does not record a host job's taint"
                    .to_owned(),
            );
        }
        let mounts = MountRoots::new(&cli.mount_roots)?;
        let scratch = cli
            .scratch_root
            .as_deref()
            .map(ScratchRoot::open)
            .transpose()?;
        let container = Settings::from_flags(
            &Flags {
                bin: cli.container_bin.as_deref(),
                network: cli.container_network.as_deref(),
                user: cli.container_user.as_deref(),
                host: cli.container_host.as_deref(),
                pull: cli.container_pull.as_deref(),
                tmp_size: cli.container_tmp_size.as_deref(),
            },
            (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
        )
        .map_err(|why| why.to_string())?;
        let state = cli.state_root.as_deref().map(StateRoot::open).transpose()?;
        if container.is_none() && !cli.watch_dirs.is_empty() {
            return Err("--watch-dir: needs --container-bin".to_owned());
        }
        if container.is_some() && state.is_none() {
            return Err(
                "--state-root: required with --container-bin: the taint marker is \
                        kept there"
                    .to_owned(),
            );
        }
        if let Some(state) = &state {
            let mut writable = mounts.paths().chain(scratch.iter().map(ScratchRoot::path));
            if writable.any(|root| state.path().starts_with(root)) {
                return Err(
                    "--state-root: lies in a --mount-root or the --scratch-root, where a \
                     job can delete or overwrite the taint marker"
                        .to_owned(),
                );
            }
        }
        let busy: Vec<&Path> = mounts
            .paths()
            .chain(scratch.iter().map(ScratchRoot::path))
            .chain(state.iter().map(StateRoot::path))
            .collect();
        Ok(Self {
            watch: watch_dirs(&cli.watch_dirs, &busy)?,
            mounts,
            scratch,
            container,
            state,
        })
    }
}

impl Local {
    /// Refuses a state root whose device number (`st_dev`) equals that of a mount root or
    /// the scratch root. A job with a read-write mount, or its own scratch directory, can
    /// fill that file system, and the taint marker could then not be written. This is a
    /// best-effort guard: the state root is to be on a disk of its own, which `st_dev` does
    /// not prove (subvolumes sharing free space have distinct device numbers), and the
    /// engine's data root and container layers are not compared.
    ///
    /// # Errors
    ///
    /// The text names the flags at fault; a path that cannot be looked at is an error too.
    pub fn check_filesystems(&self, files: &dyn FileReader) -> Result<(), String> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        let device = |path: &Path| {
            files
                .device(path)
                .map_err(|why| format!("--state-root: cannot compare file systems: {why}"))
        };
        let own = device(state.path())?;
        let roots = self
            .mounts
            .paths()
            .map(|path| ("the --mount-root", path))
            .chain(
                self.scratch
                    .iter()
                    .map(|root| ("the --scratch-root", root.path())),
            );
        for (flag, root) in roots {
            if device(root)? == own {
                return Err(format!(
                    "--state-root: on the same file system as {flag} {}: a job could fill it and the taint marker could not be written",
                    root.display()
                ));
            }
        }
        Ok(())
    }
}

/// The `--watch-dir` flags as canonical paths, sorted, each once. A directory that a job
/// writes into by design would taint the host after every job, so one that lies in or
/// holds a path of `busy` is refused.
fn watch_dirs(given: &[PathBuf], busy: &[&Path]) -> Result<Vec<PathBuf>, String> {
    let mut dirs = given
        .iter()
        .map(|path| {
            let fail = |why: &str| format!("--watch-dir {}: {why}", path.display());
            let canonical = fs::canonicalize(path)
                .ok()
                .filter(|canonical| path.is_absolute() && canonical.is_dir())
                .ok_or_else(|| fail("not an existing directory given as an absolute path"))?;
            match busy
                .iter()
                .any(|root| root.starts_with(&canonical) || canonical.starts_with(root))
            {
                true => Err(fail(
                    "lies in or holds a --mount-root, --scratch-root or --state-root, which \
                     change with every job",
                )),
                false => Ok(canonical),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}
