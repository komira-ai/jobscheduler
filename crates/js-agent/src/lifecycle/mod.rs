//! A job's life on the host: from the `JobStart` that names it, through its container, to the
//! result the server acknowledges and the slot that frees (design 5.4 and 5.10).
//!
//! [`Lifecycle`] is a state machine over three seams: a [`Runtime`], a [`CheckHook`] and a
//! [`Clock`]. It reads a [`HeartbeatReply`](js_agent_wire::HeartbeatReply) and offers the
//! [`HeldJob`](js_agent_wire::HeldJob)s and the retired job ids of the next heartbeat; it
//! reads no clock but the one it is given and sleeps never.
//!
//! # What the agent guarantees per job
//!
//! - **A start runs at most once.** It is idempotent on the job id: a start for a job the
//!   agent holds changes nothing, and so is one for a job it finished or was told to stop
//!   (the agent remembers the last [`RETIRED_CAP`] fences, each a job id with its
//!   allocation id, kept in the state root, so a late start of a stopped job does not run
//!   after a restart either).
//! - **A job start is a start keyed by job id** (`JobStart`, see `js_agent_wire::per_job`),
//!   the only start on the wire. It is made into a [`JobSpec`](js_agent_wire::JobSpec) (job id
//!   as run id, allocation id as lease id, limits the booked resources and the job's
//!   `wall_ms`) and goes through admission, refusals, slots and the wall clock: the wall
//!   limit counts from the moment the container is made, so a start delayed in delivery is
//!   neither shortened nor stretched. A job is listed in the heartbeat's `jobs`, with its
//!   result inside until a `job_acks` entry of a verdict the agent knows names it.
//!   [`Lifecycle::start_spec`] is the one start that is not on the wire: it starts a
//!   `JobSpec` as a job start is started, for the features only a `JobSpec` carries. A
//!   container the agent made is labelled ([`label::KEYING`]) `job`, and a restart adopts only
//!   a container so labelled. The job's
//!   environment is delivered: it goes to the runtime in [`RunSpec::env`], and a container
//!   runtime sets it over the image's own variables (a name in both takes the job's value),
//!   and the macOS native one over the environment it builds for the run; a runtime that
//!   cannot carry it ([`Runtime::check_env`]) answers the start `env-unsupported` and runs
//!   nothing. A
//!   `JobStop` for a held job asks it to stop (the stop grace, then the kill) and it ends
//!   `killed`, once however often it comes; for a job the agent does not hold it retires the id and lists it in
//!   `retired_jobs` (the newest [`RETIRED_LISTED`]) in every heartbeat until a reply's
//!   `retired_acks` names it, the answer that the job is stopped and gone.
//! - **A job start whose fence is retired runs never.** The fence is the job id with the
//!   allocation id (`js_agent_wire::START_FENCE_CAPABILITY`, which the hello reports). The
//!   fence of a job that ended is retired when the job leaves the listing; that of a job
//!   the server stopped, or that the agent stopped for silence, when the stop
//!   is asked, and it is in the state root before the stop is reported; a stop of a job
//!   the agent does not hold names no allocation and retires every fence of the job. Such
//!   a start is skipped (`Skip::FenceRetired`) before anything is booked or made; the same
//!   job under a new allocation is a new fence and runs.
//! - **A silence of the server stops every job** ([`Lifecycle::stop_on_silence`],
//!   `--silence-stop`, see [`DEFAULT_SILENCE_STOP`] for the bound). Contact is a welcome or
//!   a heartbeat reply the session accepted, stamped on the lifecycle's clock by the
//!   session ([`Contact`]); a reply the worker had no room for counts. Once the server has
//!   not been heard for the bound, every job that runs or waits to start is stopped as
//!   a `JobStop` stops it (the stop grace, then the kill; a native run is killed at once), its
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
//!   never wait for it.
//! - **A start waits for its image without holding the worker.** When the runtime says
//!   the image is being fetched ([`Runtime::prepare`]) the run is held as `starting` with
//!   its booking and slot and nothing else: no scratch directory, files or container (a
//!   start with layers or mounts has its directories from its admission). Each
//!   [`Lifecycle::step`] asks again, and the container is made when the image is here. A
//!   job start with layers or mounts waits too for every image of its layers, for its
//!   providers, for the lookups of its host paths and, once those are done, for its
//!   assembly ([`Runtime::resolve_fs`], [`Runtime::assemble_fs`]), each off the worker;
//!   the waits that do not depend on one another run side by side. The
//!   wall time then counts from that moment. A stop while it waits ends it without a
//!   container, and a fetch that failed ends every run that waited on it
//!   `failed_to_start`. Nothing bounds the wait but those and the runtime's own limit on
//!   a fetch, and an agent that restarts while a run waits has nothing on the host to
//!   adopt: the run is lost like any run the agent no longer holds.
//! - **A layer or a mount may come from a node-local provider**
//!   (`docs/design/mount-providers.md`, [`Providers`]). Such a start names a provider the
//!   agent was given or is refused (`input-not-allowed`); it asks each provider to `Prepare`
//!   its source when the start is admitted, beside the image pull, then writes a record of
//!   its sources in the state root (so a restart can release them; a start whose record
//!   cannot be written fails and has what was asked released), and waits (as `starting`)
//!   for each provider's `Mount` answer, the directory being held to the agent's mount
//!   roots as a host path is, for at most
//!   `--mount-provider-wait` from its admission (`input-unavailable`; a provider that refuses
//!   the ref fails it `provider-refused`). Each source is then a host path, and the start goes
//!   on as above. When the run's file system is taken apart, on every end path, each source is
//!   released; a release that fails is asked again every step for a minute and then left to
//!   the provider with a `provider-release-failed` finding, never tainting the host or holding
//!   the slot. An agent that restarts releases the sources its records name for the runs it
//!   did not adopt and sweeps each provider ([`Lifecycle::attach_providers`]).
//! - **A result says what happened.** It is sent as soon as the container has exited,
//!   before any cleanup, and it stays in the heartbeat until the server acknowledges it. An
//!   acknowledgement of a kind this version does not know keeps it. A container that
//!   exited before a stop reached it (a `JobStop` applied between the exit and the next step,
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
//!   stays blocked). The container's removal ([`Runtime::removing`]) and the removal of the
//!   run's directories ([`Lifecycle::with_cleanup_threads`]) may run off the worker: the
//!   job waits in the order, with its slot held, until the step after they answered, and a
//!   failure they answer is a failure of that step, as above. The record that the checks
//!   are owed is written on the worker before either is asked, so a restart while one runs
//!   still taints the host. When the cleanup has passed, [`Event::Cleaned`] says when each
//!   step was done and how long the worker spent on it.
//! - **A restart loses no running job.** [`Lifecycle::adopt`] finds the containers the
//!   agent labelled, before the first heartbeat of the new process, and lists them first,
//!   so the server never sees a gap that would make it run the work twice. A labelled
//!   container no job can stand for (bad or repeated labels, a booking outside the offer
//!   or beside one already taken over, a number of jobs beyond what a heartbeat can
//!   list, or a keying label other than `job`, as on a container an older release's run
//!   start made) is stopped and removed by its own id and recorded as a [`Finding`]. One that
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
//!   exited, before its cleanup and checks.) A tainted agent answers every later start
//!   `failed_to_start` with the code `host-tainted`, lets the jobs it runs finish, and
//!   offers the taint until the server acknowledges it. A marker that cannot be written
//!   keeps the agent tainted in memory (it refuses starts and frees no slot) and the taint
//!   is not reported until it is written; the marker is tried again at every step. Every
//!   taint says what changed in one line: the line the worker prints when the host is
//!   tainted names the reason, the check, the run and the check's detail
//!   ([`Lifecycle::taint_detail`] keeps the detail of the first taint for the health route).
//! - **A post-job check taints only for what a contained job can leave behind that the
//!   sandbox cannot prevent.** That is a process of a job's user that outlives the job
//!   (`process-as-job-user`), or a check that cannot run (an unreadable process table). An
//!   orphan that is not a job's process, a mount, a user or group, a kernel module or a
//!   watched directory that changed is the host's own doing (a package install while jobs
//!   run adds a group, may mount, start a service or load a module): the check returns it as a
//!   [`CheckNote`] ([`CheckHook::notes`]) and the lifecycle logs it as a [`Finding`] after
//!   every call of the check, a re-check and an exclusive lease's check included. A note
//!   never taints the host, holds a job or quarantines a slot.
//! - **A check owed is not lost with the agent.** Before the first step of a job's cleanup
//!   the agent records, in the state root, that the job's checks are owed; it removes the
//!   record once they have run (and, when they found something, once the taint is on
//!   disk). If the agent stops in between (an update's hand-over or drain that ends with
//!   a cleanup under way, a second stop, a crash), the record is found when the next
//!   process starts, and that process runs the check itself once it has taken the
//!   containers over, by the rules that need no baseline ([`CheckHook::run_restored`]):
//!   what the job left behind may be part of the baseline the new process took. A clean
//!   host removes the records and is not tainted. The host is tainted (`check-owed`)
//!   instead when the check finds a process or cannot run, when a directory of the run is
//!   still there after the start's sweep, when a record is not complete
//!   (it does not hold its run id), when the records cannot be listed or removed, or when
//!   the hook cannot judge without a baseline ([`CheckHook::checks_owed_at_start`]). A
//!   record that cannot be written holds the cleanup where it is, the container included;
//!   one that cannot be removed holds the slot. The agent never clears a `check-owed`
//!   taint (see below); an operator's clear must remove the `owed-*` records and
//!   `taint.pinned` as well as `taint.json`, or the next start taints the host again (see
//!   [`taint`]).
//! - **A taint whose cause is gone clears itself.** A taint of a process a job left, or of
//!   an input the checks could not read, is re-checked while the host is idle (every run
//!   over, `Blocked` ones included): two clean full checks `--taint-recheck-ms` apart make
//!   the agent report the clear (`taint_cleared`), and the server's acknowledgement
//!   (`taint_cleared_ack`) ends the taint, frees the blocked runs and their slots, and keeps
//!   the marker as `taint.json.cleared-<time>`. A process still there keeps the host
//!   tainted; a failed removal, owed checks and a host job are never cleared by the agent.
//!   A marker an older agent wrote for a check that no longer taints (`daemonised-child`,
//!   `new-mount`, `user-changed`, `group-changed`, `kernel-module-changed`,
//!   `watch-dir-changed`) is re-checked like a process residue and clears at the next clean
//!   re-check. A residue read from the marker at start is re-checked too, with rules that
//!   need no baseline ([`CheckHook::run_restored`]); any other restored taint stays until an
//!   operator moves the marker. See [`taint`] for the rules and
//!   `docs/design/taint-self-clearing.md` for the reasons.
//! - **A held host starts nothing** (see [`hold`]). While the hold file is in the state
//!   root (the drain of an update wrote it), every job start is answered
//!   `failed_to_start` with the code `host-held`, after the taint's refusal: the host is not
//!   tainted for it, and the file is looked at for each start, so its removal lets the next
//!   start run. A file there that is not the agent's own, or that group or others have
//!   permission on, is refused as a hold and holds the host all the same.
//! - **A `host-tainted` refusal is final for that start, and the host's.** The job did
//!   not start on this host and will not; the server must not send it to this host again
//!   (see `refusal::HOST_TAINTED`), and must treat a start refused with it as the host's
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
//! - **A blocked run leaves only with a taint that clears.** A check that finds something,
//!   or cannot run, leaves the run `Blocked`: its slot stays booked, it counts toward the
//!   runs the agent holds and is listed as `exited_unacked` for as long as the taint stands.
//!   When a taint that clears itself ends, its blocked runs are freed and their slots come
//!   back; for any other taint the host is reimaged, or an operator clears it with
//!   `js-agent clear-taint`, which keeps the marker aside and restarts the agent.
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
//! - **A `JobStop` of a job the agent does not hold retires the job id on purpose**, so
//!   that a start that arrives after it never runs.
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
//! - **Host jobs need the crun runtime.** Only the crun runtime launches a host job (an OCI
//!   container in the host's namespaces, in a cgroup of its own below its lease's; the
//!   module [`crate::crun`]), so `--allow-host-jobs` is refused at start on every other
//!   runtime ([`HOST_JOBS_NEED_CRUN`]), and only an agent that took it reports `host-jobs`:
//!   a server never taints a host for a job it cannot start. [`Config::allow_host_jobs`] is
//!   the switch the lifecycle honours: it refuses a host start without it and taints the
//!   host when such a job starts.
//! - **The FUSE device needs the crun runtime.** Only the crun runtime gives a contained job
//!   `/dev/fuse` and `CAP_SYS_ADMIN` of its own user namespace (a start with `fuse`), so
//!   `--allow-fuse-jobs` is refused at start on every other runtime
//!   ([`crate::platform::FUSE_JOBS_NEED_CRUN`]). [`Config::allow_fuse_jobs`] is the switch the
//!   lifecycle honours: it is on only where the agent reported `fuse-jobs`, and a start with
//!   `fuse` is refused without it (`fuse-jobs-not-allowed`) and creates nothing.
//!
//! # The threading contract of the runtime seam
//!
//! [`Runtime`] calls block (an image pull, the grace of a stop, a slow daemon).
//! They are made by [`Lifecycle`] only, and a [`Lifecycle`] is driven from its own worker
//! thread (see [`Worker`](crate::Worker)), never from the session's heartbeat loop, so
//! that a slow runtime call delays the next [`Lifecycle::step`] but never a heartbeat, and
//! a slow docker call cannot make the agent miss its heartbeat deadline and fence itself.
//! The worker owns the lifecycle (it is built on that thread, so the runtime needs no
//! `Send` bound); the session loop hands it replies and takes [`Lifecycle::held_jobs`],
//! [`Lifecycle::retired_jobs`] and [`Lifecycle::pending_taint`] as the snapshot the worker publishes after each order and
//! step.
//!
//! # How it is wired
//!
//! Nothing here touches the network. The worker feeds [`Lifecycle::apply_at`] each
//! heartbeat reply (stamped with the moment it arrived), calls [`Lifecycle::step`] after
//! each order and whenever it has had none for a second, and the session puts the held
//! jobs, their results and the taint in the next heartbeat. The [`Config`] is built from
//! the flags ([`Local`]) and the hello's shape ([`Capacity::of`]).

mod codes;
mod event;
mod finding;
pub mod handover;
pub mod hold;
mod job_fs;
mod machine;
mod mount_providers;
mod mounts;
mod prefetch;
mod provider;
mod quota;
mod quota_flags;
pub(crate) mod run_files;
mod runtime;
mod scratch;
pub mod scratch_volume;
mod settle;
mod silence;
pub mod taint;
mod wake;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;
use js_oci::Allow;

use crate::FileReader;
use crate::bundle::Admission;
use crate::cli::Cli;
use crate::cli_runtime::Settings;
use crate::platform::Backend;
use crate::quota::ScratchQuota;

pub use codes::{code, detail};
pub use event::{CleanupTimeline, Event, Finding, JobBooking, LookupStamps, Skip, StartTimeline};
pub use finding::{CHECK_DETAIL_MAX, CheckFinding, CheckNote, Checked};
pub use machine::{Lifecycle, Quiet, RETIRED_CAP, RETIRED_LISTED, Restored, SLOTS, Slots};
pub use mount_providers::{
    DEFAULT_PROVIDER_WAIT_SECS, MAX_PROVIDER_WAIT_SECS, MountProviders, ProviderFlags, UserNames,
};
pub use mounts::{Beneath, MountError, MountRoots};
pub use prefetch::{Forgetter, Prefetcher, PullFuture};
pub use provider::{ProviderAnswer, ProviderCall, Providers};
pub use quota::RunQuota;
pub use quota::TMP_INODES;
pub use quota_flags::{
    DEFAULT_JOB_TMP_GIB, DEFAULT_SCRATCH_ABANDON_LIMIT, DEFAULT_SCRATCH_GIB,
    DEFAULT_SCRATCH_MAX_INODES, DEFAULT_SCRATCH_QUOTA_FAILURES, DEFAULT_SCRATCH_USAGE_WAIT_SECS,
    MAX_SCRATCH_ABANDON_LIMIT, MAX_SCRATCH_GIB, MAX_SCRATCH_QUOTA_FAILURES,
    MAX_SCRATCH_USAGE_WAIT_SECS,
};
use quota_flags::{job_tmp_gib, scratch_quota_limits};
pub use run_files::{DEFAULT_ROOT as DEFAULT_RUN_FILES_ROOT, FilesRoot};
pub use runtime::{
    Body, Created, Exit, Found, FsPrepared, HostSource, JobFs, NativeRefusal, OutputsReport,
    Prepared, Removal, ResolvedMount, RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket,
    Whereabouts, check_run_env, label,
};
pub use scratch::{DirLayout, ScratchRoot};
pub use settle::{DEFAULT_SETTLE, MAX_SETTLE_MS, settle};
pub use silence::{
    Contact, DEFAULT_SILENCE_STOP, MAX_SILENCE_STOP_SECS, SILENCE_STOP_GRACE_RATIO, fits_grace,
    silence_stop,
};
pub use taint::{
    DEFAULT_TAINT_RECHECK, MARKER_FILE, MAX_TAINT_RECHECK_MS, StateRoot, clearable, recheck_every,
};
pub use wake::Wake;

/// A monotonic clock in milliseconds, from an origin of its own. The lifecycle only
/// subtracts two readings, so the origin does not matter and a change of the wall clock
/// cannot make a job run long or die early.
pub trait Clock {
    /// Milliseconds since the clock's origin.
    fn now_ms(&self) -> u64;

    /// The Unix time now in milliseconds, for a start's timeline
    /// ([`StartTimeline::received_at_ms`]) and for nothing else: no decision reads it. `None`
    /// for a clock that has none, which is what this returns unless the clock says otherwise.
    fn unix_ms(&self) -> Option<u64> {
        None
    }
}

/// A job whose container and scratch directory are gone, as the post-job checks see it.
#[derive(Debug, Clone, Copy)]
pub struct Finished<'a> {
    /// The run.
    pub run_id: &'a str,
    /// The containers of the agent's other jobs that are still on the host (running, or
    /// exited and not yet removed), those whose create is under way off the worker
    /// ([`Runtime::creating`]: the create may have made the run's control group and started
    /// its processes before the lifecycle takes its answer), and those that could not be
    /// removed. What they run and mount is not the finished job's, so a check must not count
    /// it against the job.
    pub live: &'a [RunHandle],
    /// The slot the finished run held, when it held one.
    pub slot: Option<u32>,
    /// The slots of the jobs in `live` (those whose container is still on the host).
    pub live_slots: &'a [u32],
    /// The directories under which the runtime mounts the file systems of the agent's other
    /// jobs, a job that has no container yet among them (`<mounts>/<lease>`,
    /// [`Runtime::mounts_of`]): a mount below one is that job's, not the finished one's.
    pub held_mounts: &'a [std::path::PathBuf],
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

    /// Checks the host after the job `job` as a step of the worker, which must not wait: the
    /// verdict, or [`Checked::Wait`] when the checks would let the host go on for a while
    /// before they look again. The lifecycle asks so after a job's cleanup, and asks again
    /// for the same run once the wait has passed, with its books as they are then; that
    /// call gives the verdict. The default is [`CheckHook::run`].
    fn poll(&mut self, job: &Finished<'_>) -> Checked {
        Checked::Done(self.run(job))
    }

    /// Checks the host at a re-check of a taint read from the marker at start. The baseline
    /// this process took already holds what the tainting job left, so a hook that can must
    /// also judge what the baseline cannot clear (see [`crate::integrity`]). The default is
    /// [`CheckHook::run`].
    ///
    /// # Errors
    ///
    /// As [`CheckHook::run`].
    fn run_restored(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        self.run(job)
    }

    /// Whether [`CheckHook::run_restored`] judges the host without the baseline, so that the
    /// post-job checks a run of an earlier process owed (an `owed-*` record found when the agent
    /// starts) can run at the start instead of tainting the host (see
    /// [`taint`](crate::lifecycle::taint)). The default is `false`: such a record taints the
    /// host (`check-owed`).
    fn checks_owed_at_start(&self) -> bool {
        false
    }

    /// What the last `run`, `run_restored` or `poll` that gave a verdict saw change that
    /// does not taint the host, drained by the call; none by default. The lifecycle logs
    /// each as a [`Finding`] after every such call (after a job's checks as the job's
    /// finding, with its run id; for a re-check or a whole-node lease's check with none),
    /// whatever the verdict: a note never taints the host, holds a job or quarantines a
    /// slot.
    fn notes(&mut self) -> Vec<CheckNote> {
        Vec::new()
    }

    /// The host is idle: a job is about to start, and no other job's container, scratch
    /// directory or checks are on it, nor a container the agent could not remove. Nothing a
    /// job did can change the host now, so a hook may take its baseline again. The default
    /// does nothing.
    fn idle(&mut self) {}
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
    /// Whether a start with `fuse` may run here: the agent was given `--allow-fuse-jobs` and
    /// reported `fuse-jobs` (its probe found the kernel and `/dev/fuse` fit).
    pub allow_fuse_jobs: bool,
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
    /// The `--mount-provider`, `--mount-provider-user` and `--mount-provider-wait` flags,
    /// checked: none for an agent given none.
    pub providers: MountProviders,
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
    /// `--job-tmp-gib`, or [`DEFAULT_JOB_TMP_GIB`]: the GiB of each job's temporary
    /// directory, with a runtime that gives one.
    pub job_tmp_gib: u64,
    /// `--scratch-root`, when given.
    pub scratch: Option<ScratchRoot>,
    /// `--run-files-root`, or for containers the default under `/run` when that can be
    /// made; always none without `--container-bin` or `--native-accounts`.
    pub files: Option<FilesRoot>,
    /// Why the default run files root could not be used, for a warning; none when it was
    /// used, was not needed, or `--run-files-root` was given (then a fault is an error).
    pub files_warning: Option<String>,
    /// What the agent runs jobs with (the runtime of this build, [`crate::platform`]); none
    /// for an agent that runs no job.
    pub backend: Option<Backend>,
    /// `--state-root`, when given; always given when jobs are enabled.
    pub state: Option<StateRoot>,
    /// The `--watch-dir` flags, canonicalised, sorted, each once.
    pub watch: Vec<PathBuf>,
    /// `--settle-ms`, or [`DEFAULT_SETTLE`].
    pub settle: Duration,
    /// `--taint-recheck-ms`, or [`DEFAULT_TAINT_RECHECK`].
    pub taint_recheck: Duration,
    /// `--host-second-samples`: the worker also takes the node's CPU per second.
    pub host_second_samples: bool,
    /// `--stop-grace`, or [`DEFAULT_STOP_GRACE`].
    pub stop_grace: Duration,
    /// `--drain-wait`, or the stop grace.
    pub drain_wait: Duration,
    /// The `--image-allow` flags, in the order given: where an image may be pulled from.
    pub image_allow: Vec<Allow>,
    /// The `--egress*` flags: the most network a `crun` job may ask for. Mode none unless
    /// `allow-list` or `open` was given, which only the crun runtime takes ([`crate::egress`]).
    pub egress: crate::egress::Config,
    /// `--silence-stop`, or [`DEFAULT_SILENCE_STOP`]: how long the server may be silent
    /// before every job is stopped; above the server's default grace plus the stop grace.
    pub silence_stop: Duration,
    /// The VM backend, when `--vm-driver` turned it on; the agent sets it after the checks
    /// here ([`crate::platform`]).
    pub vm: Option<crate::vm::VmSettings>,
    /// `--allow-host-jobs`, which only the crun runtime takes: the one runtime that launches
    /// a host job. The agent reports `host-jobs` only with it.
    pub allow_host_jobs: bool,
    /// `--allow-fuse-jobs`, which only the crun runtime takes: the one runtime that gives a
    /// job the FUSE device. The agent reports `fuse-jobs` only with it, and only when its
    /// probe finds the host fit ([`crate::fuse_env`]).
    pub allow_fuse_jobs: bool,
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
    /// Checks the job flags of `cli`: `--allow-host-jobs` is refused unless the runtime is
    /// `crun` ([`HOST_JOBS_NEED_CRUN`]), every mount root exists and is canonicalised,
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

    /// [`Local::from_cli`] for the crun runtime, whose settings (`crun`) stand where the
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
        let mounts = MountRoots::with_read_only(&cli.mount_roots, &cli.mount_roots_ro)?;
        // The flags of this build's runtime (see `platform`).
        let mut jobs = crate::platform::jobs(cli, crun, &mounts)?;
        let layout = jobs.layout;
        let scratch = cli
            .scratch_root
            .as_deref()
            .map(|path| ScratchRoot::open_with(path, layout))
            .transpose()?;
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
        jobs.finish(cli, &denied, state.as_ref(), euid)?;
        // The flag that turned the jobs on; an agent that runs no job takes none of the job
        // flags below.
        let needs = jobs.needs();
        if let Some(needs) = needs.filter(|_| state.is_none()) {
            return Err(format!(
                "--state-root: required with {needs}: the taint marker is kept there"
            ));
        }
        let no_jobs =
            |flag: &str| format!("{flag}: not with --no-jobs (an agent that runs no job)");
        if needs.is_none() && cli.run_files_root.is_some() {
            return Err(no_jobs("--run-files-root"));
        }
        let graces = [cli.stop_grace, cli.drain_wait];
        if needs.is_none() && graces.iter().any(Option::is_some) {
            return Err(no_jobs("--stop-grace and --drain-wait"));
        }
        let stop_grace = seconds("--stop-grace", cli.stop_grace, DEFAULT_STOP_GRACE)?;
        let drain_wait = seconds("--drain-wait", cli.drain_wait, stop_grace)?;
        if needs.is_none() && cli.silence_stop.is_some() {
            return Err(no_jobs("--silence-stop"));
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
        let denied_paths = jobs.denied_paths();
        let others: Vec<(&str, &Path)> = mounts
            .paths()
            .map(|path| ("--mount-root", path))
            .chain(scratch.iter().map(|r| ("--scratch-root", r.path())))
            .chain(state.iter().map(|r| ("--state-root", r.path())))
            .chain(denied_paths)
            .collect();
        let given = cli.run_files_root.as_deref();
        let (files, files_warning) =
            run_files::open_root(given, default_files_root, jobs.runs(), &others)?;
        let busy: Vec<&Path> = mounts
            .paths()
            .chain(scratch.iter().map(ScratchRoot::path))
            .chain(state.iter().map(StateRoot::path))
            .chain(files.iter().map(FilesRoot::path))
            .collect();
        let watch = crate::platform::Jobs::watch(cli, &busy)?;
        if let Some(needs) = needs.filter(|_| scratch.is_none()) {
            return Err(format!(
                "--scratch-root: required with {needs}: each run's private directory is made there"
            ));
        }
        let (default_gib, max_inodes, failures, usage_wait, abandon) = scratch_quota_limits(cli)?;
        let tmp_gib = job_tmp_gib(cli)?;
        let settle = jobs.settle;
        let taint_recheck = jobs.taint_recheck;
        let host_second_samples = jobs.host_second_samples;
        let egress = std::mem::take(&mut jobs.egress);
        let allow_host_jobs = jobs.allow_host_jobs;
        let allow_fuse_jobs = jobs.allow_fuse_jobs;
        let providers = jobs.providers();
        Ok(Self {
            watch,
            mounts,
            providers,
            scratch_quota: cli.scratch_quota,
            scratch_default_gib: default_gib,
            scratch_max_inodes: max_inodes,
            scratch_quota_failures: failures,
            scratch_usage_wait_secs: usage_wait,
            scratch_abandon_limit: abandon,
            job_tmp_gib: tmp_gib,
            backend: jobs.backend(scratch.as_ref(), files.as_ref()),
            scratch,
            files,
            files_warning,
            state,
            settle,
            taint_recheck,
            host_second_samples,
            stop_grace,
            drain_wait,
            image_allow: image_allow(&cli.image_allow)?,
            egress,
            silence_stop,
            vm: None,
            allow_host_jobs,
            allow_fuse_jobs,
        })
    }
}

/// The `--image-allow` entries, each checked.
pub(crate) fn image_allow(entries: &[String]) -> Result<Vec<Allow>, String> {
    entries
        .iter()
        .map(|entry| Allow::parse(entry).map_err(|why| format!("--image-allow: {why}")))
        .collect()
}

impl Local {
    /// The quota the flags ask for over `quota`: with `bounds_tmp` (a runtime that gives each
    /// job a temporary directory), each job's temporary directory is bounded to
    /// `--job-tmp-gib`; with `--scratch-quota`, a run without `disk_bytes` books
    /// `--scratch-default-gib`, and a run's scratch is bounded to the bytes it books and to
    /// `--scratch-max-inodes` inodes.
    pub fn run_quota(&self, quota: Arc<dyn ScratchQuota>, bounds_tmp: bool) -> RunQuota {
        let quota = match self.scratch_quota {
            true => RunQuota::new(
                quota,
                self.scratch_default_gib << 30,
                self.scratch_max_inodes,
            ),
            false => RunQuota::tmp_only(quota),
        };
        let quota = match bounds_tmp {
            true => quota.with_tmp_bytes(self.job_tmp_gib << 30),
            false => quota.without_tmp(),
        };
        quota
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
        if let Some(root) = self
            .files
            .as_ref()
            .filter(|_| crate::platform::RUN_FILES_ON_TMPFS)
        {
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
#[cfg(target_os = "linux")]
pub(crate) fn watch_dirs(given: &[PathBuf], busy: &[&Path]) -> Result<Vec<PathBuf>, String> {
    let mut dirs = given
        .iter()
        .map(|path| {
            let fail = |why: &str| format!("--watch-dir {}: {why}", path.display());
            let canonical = std::fs::canonicalize(path)
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
