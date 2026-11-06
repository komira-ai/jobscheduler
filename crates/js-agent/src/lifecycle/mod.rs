//! A job's life on the host: from the `Start` that names it, through its container, to the
//! result the server acknowledges and the slot that frees (design 5.4 and 5.10).
//!
//! [`Lifecycle`] is a state machine over three seams: a [`Runtime`], a [`CheckHook`] and a
//! [`Clock`]. It reads a [`HeartbeatReply`](js_agent_wire::HeartbeatReply) and offers the
//! [`HeldRun`](js_agent_wire::HeldRun)s and [`RunResult`](js_agent_wire::RunResult)s, the
//! [`HeldJob`](js_agent_wire::HeldJob)s and the retired job ids of the next heartbeat; it
//! reads no clock but the one it is given and sleeps never.
//!
//! # What the agent guarantees per job
//!
//! - **A start runs at most once.** It is idempotent on the run id: a start for a run the
//!   agent holds changes nothing, and so is one for a run it finished or was told to stop
//!   (the agent remembers the last [`RETIRED_CAP`] run ids). A start that reaches the
//!   agent later than its `valid_for_ms` allows (the agent's clock, measured from the
//!   moment the reply arrived) is dropped and nothing is sent.
//! - **A job start is a start keyed by job id** (`JobStart`, see `js_agent_wire::per_job`).
//!   It is made into the start a run start makes (run id the job id, lease id the
//!   allocation id, limits the booked resources and the job's `wall_ms`, and no
//!   `valid_for_ms`, so no lease bounds it) and goes through the same admission, refusals,
//!   slots and wall clock: the wall limit counts from the moment the container is made, so a
//!   start delayed in delivery is neither shortened nor stretched. It is idempotent on the
//!   job id, and job ids share the bounded set of retired run ids above (kept in the state
//!   root, so a late start of a stopped job does not run after a restart either), each with
//!   its allocation: a job start is judged by its fence (below). Such a
//!   job is listed in the heartbeat's `jobs` alone, with its
//!   result inside until a `job_acks` entry of a verdict the agent knows names it; never in
//!   `held` or `results`, and no `Stop`, `Cancel` or `result_acks` entry touches it (nor a
//!   `JobStop` or `job_acks` entry a run), so a lease server never sees a job id. A start
//!   whose id names held work of the other keying is not taken (`Skip::Clash`). The job's
//!   environment is delivered: it goes to the runtime in [`RunSpec::env`], and a container
//!   runtime sets it over the image's own variables (a name in both takes the job's value);
//!   a runtime that cannot carry it (the macOS native one) answers the start
//!   `env-unsupported` and runs nothing. A
//!   `JobStop` for a held job ends it as a `Stop` does (`killed`, once however often it
//!   comes); for a job the agent does not hold it retires the id and lists it in
//!   `retired_jobs` (the newest [`RETIRED_LISTED`]) in every heartbeat until a reply's
//!   `retired_acks` names it, the answer that the job is stopped and gone. A container a
//!   job start made is labelled ([`label::KEYING`]), so a restart that adopts it
//!   lists it as a job again.
//! - **A job start whose fence is retired runs never.** The fence is the job id with the
//!   allocation id (`js_agent_wire::START_FENCE_CAPABILITY`, which the hello reports). The
//!   fence of a job that ended is retired when the job leaves the listing; that of a job
//!   the server stopped or cancelled, or that the agent stopped for silence, when the stop
//!   is asked, and it is in the state root before the stop is reported; a stop of a job
//!   the agent does not hold names no allocation and retires every fence of the job. Such
//!   a start is skipped (`Skip::FenceRetired`) before anything is booked or made; the same
//!   job under a new allocation is a new fence and runs. A run start has no fence beyond
//!   its run id and is judged as before (`Skip::Retired`).
//! - **A silence of the server stops every job** ([`Lifecycle::stop_on_silence`],
//!   `--silence-stop`, see [`DEFAULT_SILENCE_STOP`] for the bound). Contact is a welcome or
//!   a heartbeat reply the session accepted, stamped on the lifecycle's clock by the
//!   session ([`Contact`]); a reply the worker had no room for counts. Once the server has
//!   not been heard for the bound, every job that runs or waits for its image is stopped as
//!   a `Stop` stops it (the stop grace, then the kill; a native run is killed at once), its
//!   fence is retired and its result is `killed` with the detail `silence-stop`. It is done
//!   once for each silence; a contact ends the silence, and starts that follow are judged
//!   as above. It runs while an agent runs: a native run is not bounded while no agent
//!   does (see [`crate::native`]).
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
//! - **Live logs are outside the job's critical path.** A job start's checked sink goes to
//!   the runtime ([`RunSpec::log_sink`]; one that fails its check is dropped with a
//!   `log-sink-ignored` note and the job starts as without one), and a runtime that uploads
//!   (`crun` and the native runtime, [`crate::logs`]) does so on a thread of its own per job:
//!   the result, with its tail and `logs` ([`Runtime::logs_report`]), the cleanup and the slot
//!   never wait for it. At the start, before the scratch root is swept, the runtime is handed
//!   the directories the sweep removes ([`Runtime::finish_left_uploads`]), so the native one
//!   finishes the uploads of the runs its journal ended.
//! - **A start waits for its image without holding the worker.** When the runtime says
//!   the image is being fetched ([`Runtime::prepare`]) the run is held as `starting` with
//!   its booking and slot and nothing else: no scratch directory, files or container. Each
//!   [`Lifecycle::step`] asks again, and the container is made when the image is here. The
//!   wall time then counts from that moment, but never past the end of the lease the
//!   start's `valid_for_ms` named. A stop or cancel while it waits ends it without a
//!   container, and a fetch that failed ends every run that waited on it
//!   `failed_to_start`. Nothing bounds the wait but those and the runtime's own limit on
//!   a fetch, and an agent that restarts while a run waits has nothing on the host to
//!   adopt: the run is lost like any run the agent no longer holds.
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
//! - **A scratch quota bounds the run before anything is written** (`--scratch-quota`, see
//!   [`RunQuota`]). A run books its own `disk_bytes` or the quota's default (the number the
//!   server books for it too) and is limited to the bytes it books and to an inode limit.
//!   The slot's project is claimed before anything is made (a project that still holds
//!   usage parks the slot and refuses the run `no-capacity`), the limits are set on the empty
//!   scratch directory before the container is created, and at the end the project is
//!   cleared after the tree is removed; a failed clear holds the slot and is retried at every
//!   step. The sweep of a restart clears the projects of the leftover directories it removes.
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
//! - **Held results and retired fences survive a restart** as files in the state root (see
//!   `held`); the `retired_jobs` still owed the server do not. Containers that still exist
//!   are re-adopted, an exited one as its container says.
//! - **A `Stop` or `Cancel` of a run the agent does not hold retires the run id on
//!   purpose**, so that a `Start` that arrives after it never runs.
//! - **Each run has a slot, and each slot a user of its own.** A run in slot `k` (the lowest
//!   free one, from 0) runs as the base of `--container-user` plus `k`, so runs held at once
//!   never share a host uid. A slot is free again only when the cleanup of its last run
//!   passed (container and scratch gone, the post-job check clean); when the check finds
//!   something, or a container cannot be removed, the slot is quarantined and takes no run
//!   for as long as the agent lives (the host is tainted then, and refuses starts, as
//!   before). A restart reads a run's slot from its container's label; a container
//!   without the label (an earlier agent made it) is taken over holding no slot, so its
//!   uid is not protected. See [`Slots`].
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
//! [`Runtime`] calls block (an image pull, the grace of a stop, a slow daemon).
//! They are made by [`Lifecycle`] only, and a [`Lifecycle`] is driven from its own worker
//! thread (see [`Worker`](crate::Worker)), never from the session's heartbeat loop, so
//! that a slow runtime call delays the next [`Lifecycle::step`] but never a heartbeat, and
//! a slow docker call cannot make the agent miss its heartbeat deadline and fence itself.
//! The worker owns the lifecycle (it is built on that thread, so the runtime needs no
//! `Send` bound); the session loop hands it replies and takes [`Lifecycle::held`],
//! [`Lifecycle::results`], [`Lifecycle::held_jobs`], [`Lifecycle::retired_jobs`] and
//! [`Lifecycle::pending_taint`] as the snapshot the worker publishes after each order and
//! step.
//!
//! # How it is wired
//!
//! Nothing here touches the network. The worker feeds [`Lifecycle::apply_at`] each
//! heartbeat reply (stamped with the moment it arrived), calls [`Lifecycle::step`] after
//! each order and whenever it has had none for a second, and the session puts the held
//! runs, the results and the taint in the next heartbeat. The [`Config`] is built from
//! the flags ([`Local`]) and the hello's shape ([`Capacity::of`]).

mod codes;
mod event;
mod machine;
mod mounts;
mod prefetch;
mod quota;
mod quota_flags;
mod run_files;
mod runtime;
mod scratch;
mod silence;
pub mod taint;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;
use js_oci::Allow;

use crate::FileReader;
use crate::bundle::Admission;
use crate::cli::Cli;
use crate::cli_runtime::{Flags, Settings};
use crate::native::{NativeFlags, NativeSettings, PfFlags};
use crate::quota::ScratchQuota;

pub use codes::{code, detail};
pub use event::{Event, Finding, Skip};
pub use machine::{
    Lifecycle, Quiet, RETIRED_CAP, RETIRED_LISTED, Restored, RunningLease, SLOTS, Slots,
};
pub use mounts::{MountError, MountRoots};
pub use prefetch::{Prefetcher, PullFuture};
pub use quota::RunQuota;
use quota_flags::scratch_quota_limits;
pub use quota_flags::{
    DEFAULT_SCRATCH_ABANDON_LIMIT, DEFAULT_SCRATCH_GIB, DEFAULT_SCRATCH_MAX_INODES,
    DEFAULT_SCRATCH_QUOTA_FAILURES, DEFAULT_SCRATCH_USAGE_WAIT_SECS, MAX_SCRATCH_ABANDON_LIMIT,
    MAX_SCRATCH_GIB, MAX_SCRATCH_QUOTA_FAILURES, MAX_SCRATCH_USAGE_WAIT_SECS,
};
pub use run_files::{DEFAULT_ROOT as DEFAULT_RUN_FILES_ROOT, FilesRoot};
pub use runtime::{
    Body, Exit, Found, NativeRefusal, Prepared, ResolvedMount, RunHandle, RunSpec, Runtime,
    RuntimeError, Status, Ticket, check_run_env, label,
};
pub use scratch::{DirLayout, ScratchRoot};
pub use silence::{
    Contact, DEFAULT_SILENCE_STOP, MAX_SILENCE_STOP_SECS, SILENCE_STOP_GRACE_RATIO, fits_grace,
    silence_stop,
};
pub use taint::{MARKER_FILE, StateRoot};

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
    pub live: &'a [RunHandle],
    /// The slot the finished run held, when it held one.
    pub slot: Option<u32>,
    /// The slots of the jobs in `live` (those whose container is still on the host).
    pub live_slots: &'a [u32],
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

    /// The host is idle: a job is about to start, and no other job's container, scratch
    /// directory or checks are on it, nor a container the agent could not remove. Nothing a
    /// job did can change the host now, so a hook may take its baseline again. The default
    /// does nothing.
    fn idle(&mut self) {}
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

/// What a host offers jobs: the shape of its hello and the GPU devices it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capacity {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Bytes of scratch disk.
    pub disk_bytes: u64,
    /// The GPU device indices on offer.
    pub gpus: BTreeSet<u32>,
}

impl Capacity {
    /// The capacity a hello offers.
    pub fn of(hello: &Hello) -> Self {
        Self {
            cpu_millis: hello.shape.cpu_millis,
            memory_bytes: hello.shape.memory_bytes,
            disk_bytes: hello.shape.disk_bytes,
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
    /// Where the runs' files go (on a tmpfs, for containers); none when the agent was started
    /// without a usable `--run-files-root`, and then a start with files is refused.
    pub files: Option<FilesRoot>,
    /// Where the taint marker is kept.
    pub state: StateRoot,
    /// How long a container gets to stop on its own before it is killed.
    pub stop_grace: Duration,
    /// The rules for a start that names a bundle; none when the agent has no bundle store,
    /// and then such a start is refused `bundle-unsupported`.
    pub bundle: Option<Admission>,
    /// The disk quota of the runs' scratch directories; none without `--scratch-quota`, and
    /// then a run without `disk_bytes` books none and nothing bounds a run's disk.
    pub quota: Option<RunQuota>,
}

impl Config {
    /// The bytes of disk a run with `limits` books: its own `disk_bytes`, or the quota's
    /// default for a run that names none (the number the server books for it too), or 0
    /// without a quota.
    pub fn disk_of(&self, limits: &js_agent_wire::Limits) -> u64 {
        let default = self.quota.as_ref().map_or(0, RunQuota::default_disk_bytes);
        limits.disk_bytes.unwrap_or(default)
    }
}

/// The flags that configure jobs, checked.
#[derive(Debug, Clone)]
pub struct Local {
    /// The `--mount-root` and `--mount-root-ro` flags, canonicalised.
    pub mounts: MountRoots,
    /// `--scratch-quota` was given.
    pub scratch_quota: bool,
    /// `--scratch-default-gib`, or [`DEFAULT_SCRATCH_GIB`]: the GiB a run without
    /// `disk_bytes` books. Meaningful with `--scratch-quota` only.
    pub scratch_default_gib: u64,
    /// `--scratch-max-inodes`, or [`DEFAULT_SCRATCH_MAX_INODES`]: the inode limit of a
    /// run. Meaningful with `--scratch-quota` only.
    pub scratch_max_inodes: u64,
    /// `--scratch-quota-failures`, or [`DEFAULT_SCRATCH_QUOTA_FAILURES`]. Meaningful with
    /// `--scratch-quota` only.
    pub scratch_quota_failures: u32,
    /// `--scratch-usage-wait-secs`, or [`DEFAULT_SCRATCH_USAGE_WAIT_SECS`]. Meaningful with
    /// `--scratch-quota` only.
    pub scratch_usage_wait_secs: u64,
    /// `--scratch-abandon-limit`, or [`DEFAULT_SCRATCH_ABANDON_LIMIT`]. Meaningful with
    /// `--scratch-quota` only.
    pub scratch_abandon_limit: u32,
    /// `--scratch-root`, when given.
    pub scratch: Option<ScratchRoot>,
    /// `--run-files-root`, or for containers the default under `/run` when that can be
    /// made; always none without `--container-bin` or `--native-helper-socket`.
    pub files: Option<FilesRoot>,
    /// Why the default run files root could not be used, for a warning; none when it was
    /// used, was not needed, or `--run-files-root` was given (then a fault is an error).
    pub files_warning: Option<String>,
    /// `--container-bin` and the container flags beside it, when containers are enabled by
    /// `--container-bin`.
    pub container: Option<Settings>,
    /// `--native-helper-socket` and the flags beside it, when native jobs are enabled; never
    /// together with [`Local::container`].
    pub native: Option<NativeSettings>,
    /// `--state-root`, when given; always given when jobs are enabled.
    pub state: Option<StateRoot>,
    /// The `--watch-dir` flags, canonicalised, sorted, each once.
    pub watch: Vec<PathBuf>,
    /// `--stop-grace`, or [`DEFAULT_STOP_GRACE`].
    pub stop_grace: Duration,
    /// `--drain-wait`, or the stop grace.
    pub drain_wait: Duration,
    /// The `--image-allow` flags, in the order given: where an image may be pulled from.
    pub image_allow: Vec<Allow>,
    /// The `--egress*` flags: the most network a `crun` job may ask for. Mode none unless
    /// `allow-list` or `open` was given, which only `--runtime crun` takes ([`crate::egress`]).
    pub egress: crate::egress::Config,
    /// `--silence-stop`, or [`DEFAULT_SILENCE_STOP`]: how long the server may be silent
    /// before every job is stopped; above the server's default grace plus the stop grace.
    pub silence_stop: Duration,
}

/// The file in the state root that holds the native agent's lease journal.
pub const NATIVE_JOURNAL: &str = "native-leases";

/// How long a container gets to stop on its own when `--stop-grace` is not given.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

/// The most seconds `--stop-grace` and `--drain-wait` take.
pub const MAX_GRACE_SECS: u64 = 3600;

/// A number of seconds a flag gave, or `default`.
fn seconds(flag: &str, given: Option<u64>, default: Duration) -> Result<Duration, String> {
    given.map_or(Ok(default), |secs| match secs <= MAX_GRACE_SECS {
        true => Ok(Duration::from_secs(secs)),
        false => Err(format!("{flag}: at most {MAX_GRACE_SECS} seconds")),
    })
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
    /// `euid` is the effective user id of the agent: a native agent must be root.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_cli(cli: &Cli, euid: u32) -> Result<Self, String> {
        Self::from_cli_with_default_files_root(cli, Path::new(DEFAULT_RUN_FILES_ROOT), euid)
    }

    /// [`Local::from_cli`] with `default_files_root` where the run files go when
    /// `--run-files-root` is not given (a seam for tests; the agent uses
    /// [`DEFAULT_RUN_FILES_ROOT`]).
    ///
    /// # Errors
    ///
    /// As [`Local::from_cli`].
    pub fn from_cli_with_default_files_root(
        cli: &Cli,
        default_files_root: &Path,
        euid: u32,
    ) -> Result<Self, String> {
        Self::from_parts(cli, default_files_root, euid, None)
    }

    /// [`Local::from_cli`] for `--runtime crun`, whose settings (`crun`) stand where the
    /// container flags make them for an engine. The scratch root and the run files root are
    /// [`DirLayout::Searchable`] (the job's ids walk down to them); the image store is the
    /// directory beside the scratch root, and the settings are told where it is.
    ///
    /// # Errors
    ///
    /// As [`Local::from_cli`], and the scratch root has no parent for the image store.
    pub fn from_cli_crun(cli: &Cli, euid: u32, crun: Settings) -> Result<Self, String> {
        Self::from_parts(cli, Path::new(DEFAULT_RUN_FILES_ROOT), euid, Some(crun))
    }

    fn from_parts(
        cli: &Cli,
        default_files_root: &Path,
        euid: u32,
        crun: Option<Settings>,
    ) -> Result<Self, String> {
        let layout = match crun {
            Some(_) => DirLayout::Searchable,
            None => DirLayout::Private,
        };
        if cli.allow_host_jobs {
            return Err(
                "--allow-host-jobs: host jobs are not available yet: the scheduler \
                        does not record a host job's taint"
                    .to_owned(),
            );
        }
        let egress = egress_of(cli, crun.is_some())?;
        let mounts = MountRoots::with_read_only(&cli.mount_roots, &cli.mount_roots_ro)?;
        let scratch = cli
            .scratch_root
            .as_deref()
            .map(|path| ScratchRoot::open_with(path, layout))
            .transpose()?;
        let from_flags = Settings::from_flags(
            &Flags {
                bin: cli.container_bin.as_deref(),
                user: cli.container_user.as_deref(),
                host: cli.container_host.as_deref(),
                pull: cli.container_pull.as_deref(),
                tmp_size: cli.container_tmp_size.as_deref(),
                add_hosts: &cli.container_add_host,
                actions_cgroup: cli.actions_cgroup.as_deref(),
                subuid: cli.container_subuid.as_deref(),
            },
            (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
        )
        .map_err(|why| why.to_string())?;
        let is_crun = crun.is_some();
        let needs = if is_crun {
            "--runtime crun"
        } else {
            "--container-bin"
        };
        let container = crun.or(from_flags);
        let state = cli.state_root.as_deref().map(StateRoot::open).transpose()?;
        // What no run may read: the agent's own roots and the files it keeps secrets in.
        let denied: Vec<&Path> = state
            .iter()
            .map(StateRoot::path)
            .chain(scratch.iter().map(ScratchRoot::path))
            .chain(cli.token_file.as_deref())
            .chain(cli.instance_file.as_deref())
            .chain(cli.ca_cert.as_deref())
            .collect();
        let journal = state
            .as_ref()
            .map_or_else(PathBuf::new, |state| state.path().join(NATIVE_JOURNAL));
        let native = NativeSettings::from_flags(
            &NativeFlags {
                socket: cli.native_helper_socket.as_deref(),
                homes: cli.native_homes.as_deref(),
                network: cli.native_network.as_deref(),
                tool_roots: &cli.native_tool_roots,
                deny_read: &cli.native_deny_read,
                max_file_mib: cli.native_max_file_mib,
                max_processes: cli.native_max_processes,
                pf: PfFlags {
                    pfctl: cli.native_pfctl.as_deref(),
                    anchor: cli.native_pf_anchor.as_deref(),
                    state_dir: cli.native_pf_state_dir.as_deref(),
                    uid_start: cli.native_pf_uid_start,
                    uid_count: cli.native_pf_uid_count,
                    endpoints: &cli.native_pf_endpoints,
                },
            },
            &denied,
            &journal,
        )?;
        if native.is_some() && euid != 0 {
            return Err(
                "--native-helper-socket: the agent must run as root: it gives each run's \
                 directory to the run's user"
                    .to_owned(),
            );
        }
        if container.is_some() && native.is_some() {
            return Err(
                "--native-helper-socket: not with --container-bin: an agent runs \
                        containers or native programs, not both"
                    .to_owned(),
            );
        }
        if native.is_some() && state.is_none() {
            return Err(
                "--state-root: required with --native-helper-socket: the taint \
                        marker is kept there"
                    .to_owned(),
            );
        }
        if native.is_some() && scratch.is_none() {
            return Err(
                "--scratch-root: required with --native-helper-socket: each run's \
                        private directory is made there"
                    .to_owned(),
            );
        }
        if container.is_none() && !cli.watch_dirs.is_empty() {
            return Err("--watch-dir: needs --container-bin".to_owned());
        }
        if container.is_none() && native.is_none() && cli.run_files_root.is_some() {
            return Err("--run-files-root: needs --container-bin or --native-helper-socket".into());
        }
        if container.is_some() && state.is_none() {
            return Err(format!(
                "--state-root: required with {needs}: the taint marker is kept there"
            ));
        }
        let graces = [cli.stop_grace, cli.drain_wait];
        if container.is_none() && native.is_none() && graces.iter().any(Option::is_some) {
            return Err("--stop-grace and --drain-wait: need --container-bin".to_owned());
        }
        let stop_grace = seconds("--stop-grace", cli.stop_grace, DEFAULT_STOP_GRACE)?;
        let drain_wait = seconds("--drain-wait", cli.drain_wait, stop_grace)?;
        if container.is_none() && native.is_none() && cli.silence_stop.is_some() {
            return Err(
                "--silence-stop: needs --container-bin or --native-helper-socket".to_owned(),
            );
        }
        let silence_stop = silence::silence_stop(cli.silence_stop)?;
        silence::fits_grace(silence_stop, js_agent_wire::DEFAULT_GRACE_MS, stop_grace)?;
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
        // A native agent's files root may also not be, hold or lie in a path its profiles
        // deny reads of (the job could not read its files) or the homes of its users.
        let native_paths = native.iter().flat_map(|native| {
            let config = native.config();
            let denies = config
                .read_denies
                .iter()
                .map(|path| Path::new(path.as_str()));
            let denies = denies.map(|path| ("path no native run may read", path));
            denies.chain([("--native-homes", config.homes.as_path())])
        });
        let others: Vec<(&str, &Path)> = mounts
            .paths()
            .map(|path| ("--mount-root", path))
            .chain(scratch.iter().map(|r| ("--scratch-root", r.path())))
            .chain(state.iter().map(|r| ("--state-root", r.path())))
            .chain(native_paths)
            .collect();
        let runs = match (&container, &native) {
            (Some(_), _) => run_files::Runs::Containers(layout),
            (None, Some(_)) => run_files::Runs::Native,
            (None, None) => run_files::Runs::None,
        };
        let given = cli.run_files_root.as_deref();
        let (files, files_warning) =
            run_files::open_root(given, default_files_root, runs, &others)?;
        let busy: Vec<&Path> = mounts
            .paths()
            .chain(scratch.iter().map(ScratchRoot::path))
            .chain(state.iter().map(StateRoot::path))
            .chain(files.iter().map(FilesRoot::path))
            .collect();
        let watch = watch_dirs(&cli.watch_dirs, &busy)?;
        if container.is_some() && scratch.is_none() {
            return Err(format!(
                "--scratch-root: required with {needs}: each run's private directory is made there"
            ));
        }
        let container = match (is_crun, &scratch) {
            (true, Some(root)) => {
                container.map(|settings| settings.placed_beside(root.path(), files.clone()))
            }
            _ => container,
        };
        let (default_gib, max_inodes, failures, usage_wait, abandon) = scratch_quota_limits(cli)?;
        Ok(Self {
            watch,
            mounts,
            scratch_quota: cli.scratch_quota,
            scratch_default_gib: default_gib,
            scratch_max_inodes: max_inodes,
            scratch_quota_failures: failures,
            scratch_usage_wait_secs: usage_wait,
            scratch_abandon_limit: abandon,
            scratch,
            files,
            files_warning,
            container,
            native,
            state,
            stop_grace,
            drain_wait,
            image_allow: image_allow(&cli.image_allow)?,
            egress,
            silence_stop,
        })
    }
}

/// The `--egress*` flags, checked, for an agent whose runtime is `crun` when `crun`. Only
/// `crun` enforces a ceiling, so a ceiling above none on another runtime is refused rather
/// than run with no network at all.
fn egress_of(cli: &Cli, crun: bool) -> Result<crate::egress::Config, String> {
    use crate::egress::{Config, Flags, Mode};
    let config = Config::from_flags(&Flags {
        mode: cli.egress.as_deref(),
        allow: &cli.egress_allow,
        dns: &cli.egress_dns,
        forward_ports: &cli.egress_forward_port,
        pool: cli.egress_pool.as_deref(),
        ip: cli.egress_ip.as_deref(),
        nft: cli.egress_nft.as_deref(),
        nsenter: cli.egress_nsenter.as_deref(),
    })
    .map_err(|why| why.to_string())?;
    if config.mode() != Mode::None && !crun {
        return Err(format!(
            "--egress {}: needs --runtime crun, the only runtime that enforces it; a job run \
             through --container-bin has network none",
            config.mode()
        ));
    }
    Ok(config)
}

/// The `--image-allow` entries, each checked.
pub(crate) fn image_allow(entries: &[String]) -> Result<Vec<Allow>, String> {
    entries
        .iter()
        .map(|entry| Allow::parse(entry).map_err(|why| format!("--image-allow: {why}")))
        .collect()
}

impl Local {
    /// The quota the flags ask for over `quota`: a run without `disk_bytes` books
    /// `--scratch-default-gib`, and a run is bounded to the bytes it books and to
    /// `--scratch-max-inodes` inodes.
    pub fn run_quota(&self, quota: Arc<dyn ScratchQuota>) -> RunQuota {
        RunQuota::new(
            quota,
            self.scratch_default_gib << 30,
            self.scratch_max_inodes,
        )
        .with_max_failures(self.scratch_quota_failures)
        .with_usage_wait_ms(self.scratch_usage_wait_secs.saturating_mul(1000))
        .with_abandon_limit(self.scratch_abandon_limit)
    }

    /// Refuses a state root whose device number (`st_dev`) equals that of a mount root or
    /// the scratch root. A job with a read-write mount, or its own scratch directory, can
    /// fill that file system, and the taint marker could then not be written. This is a
    /// best-effort guard: the state root is to be on a disk of its own, which `st_dev` does
    /// not prove (subvolumes sharing free space have distinct device numbers), and the
    /// engine's data root and container layers are not compared.
    ///
    /// The run files root of a container agent (`--run-files-root`, or the default under
    /// `/run`) must be on a `tmpfs`; one that is not, or cannot be looked at, stops the agent.
    ///
    /// # Errors
    ///
    /// The text names the flags at fault; a path that cannot be looked at is an error too.
    pub fn check_filesystems(&self, files: &dyn FileReader) -> Result<(), String> {
        // A Mac has no tmpfs: a native agent's root is wherever the operator put it.
        if let Some(root) = self.files.as_ref().filter(|_| self.native.is_none()) {
            let on_tmpfs = files.is_tmpfs(root.path()).map_err(|why| {
                format!("--run-files-root: cannot tell what file system it is on: {why}")
            })?;
            if !on_tmpfs {
                return Err(format!(
                    "--run-files-root: {} is not on a tmpfs: a run's files hold secrets that \
                     must not reach a disk",
                    root.path().display()
                ));
            }
        }
        self.check_mount_roots_off_scratch(files)?;
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

impl Local {
    /// With `--scratch-quota`, refuses a `--mount-root` on the file system of the scratch
    /// root (the same `st_dev` comparison as the state root's): a job with a read-write
    /// mount there writes to the disk the quota is to bound, outside it. `--mount-root-ro`
    /// roots are not compared: a mount from one is read-only. Without the flag nothing is
    /// compared.
    fn check_mount_roots_off_scratch(&self, files: &dyn FileReader) -> Result<(), String> {
        let (true, Some(scratch)) = (self.scratch_quota, &self.scratch) else {
            return Ok(());
        };
        let device = |path: &Path| {
            files
                .device(path)
                .map_err(|why| format!("--scratch-quota: cannot compare file systems: {why}"))
        };
        let own = device(scratch.path())?;
        for root in self.mounts.writable_paths() {
            if device(root)? == own {
                return Err(format!(
                    "--mount-root: {} is on the same file system as the --scratch-root, which \
                     --scratch-quota bounds: a read-write mount there is outside the quota; \
                     give it as --mount-root-ro or move it",
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
