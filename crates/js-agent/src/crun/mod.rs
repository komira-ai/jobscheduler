//! The crun container runtime: [`CrunRuntime`], a [`Runtime`] over js-contain's `crun`
//! backend, for a root agent that starts every contained job's container as a dedicated
//! non-root job user (a host job's runs as the agent: see "Host jobs" below).
//!
//! # What a run becomes
//!
//! A run is one *lease*, named `js-<agent tag>-<run id>` (the name the other runtime gives
//! a container, so the two never name one alike). Its pieces:
//!
//! - the lease directory and its `root`, `upper`, `work` and `rw/home` directories, made
//!   `0755` by the agent and given to the job user (the account `crun` keeps its state and
//!   log as) or to the job's own host id (what the job writes). The agent's lease directory
//!   is `.lease-<name>` in the scratch root, beside the run's own directory
//!   ([`Runtime::lease_dir`]), and the lifecycle puts it under the run's project quota, so one
//!   bound covers `/scratch`, `/work` and `/root`; a runtime opened without
//!   `OciConfig::leases_in` keeps it as `<store>/<name>`;
//! - the job's `.tmp-<name>` beside it ([`Runtime::tmp_dir`]): its `/tmp` and `/var/tmp`;
//! - the cgroup `<actions>/<name>`, made by the agent from the booking (no memory or swap
//!   limit: the memory booking is what placement counted, and the job's use is measured and
//!   reported, not held at it; the booked cores as the lease's `cpu.weight`) and given to the
//!   job user, who makes the container's cgroup
//!   `<name>/ctr` under it ([`Runtime::cgroup`]). The job user is given nothing of the
//!   directory the leases' cgroups are in (it could make or remove the cgroups of other
//!   leases there) except the cgroup `helpers` below it, where the helper containers are
//!   made, and one file of the agent's own cgroup, `cgroup.procs`: `crun` moves its first
//!   process from the agent's cgroup into the container's, which the kernel allows with
//!   write access to the `cgroup.procs` of their nearest common ancestor;
//!   The write to the agent root's `cgroup.procs` lets the job uid move any process of the
//!   agent's own cgroup (the agent included) into a cgroup it owns: that is the kernel's rule
//!   for moving a process between cgroups, not something to narrow, and only host-side
//!   `crun` and the helpers run as the job uid. `cgroup.kill` of a lease's cgroup is left
//!   out of [`JOB_OWNED_FILES`]: only the agent ends a container through it (a stop's kill,
//!   a removal), and a job user able to write it could end the lease from the host side.
//! - the run's scratch directory, given to the job's host id and bound at `/scratch`; the
//!   run's mounts, and its files at `/run/js-files` read-only, are bound after it;
//! - a bundle of the image's unpacked root (read-only; the assembled root, writable, for a
//!   start with layers), an overlay of an empty input root at `/work` (unless a mount takes
//!   its place), where the job starts, the image config's program with the run's arguments
//!   after the entrypoint, and its environment with the job's ([`RunSpec::env`]) over it.
//!
//! `crun run` is run by a *supervisor*, a process of its own (the agent's binary in its
//! `__supervise-run` helper mode, started as the job user (as the agent for a host job) in
//! its own process group with SIGHUP, SIGINT and SIGTERM blocked), not by a thread of the
//! agent. The supervisor owns
//! the container's pipes and log ring, and writes the container's end (its exit, and the
//! tail of what it wrote) to an exit record in the lease directory when the run is over. An
//! agent that exits, gracefully or not, leaves its supervisors and containers running (the
//! unit's `KillMode=process` ends only the agent). The runtime keeps a handle on each
//! ([`Supervised`]) and reads the end from the record; a container exists from its creation
//! to its [`Runtime::remove`], and [`Runtime::list_labelled`] lists those the runtime holds,
//! the ones it adopted when it opened among them. The wall limit is enforced only while an
//! agent runs: the time a job ran with no agent counts against it, and a job whose limit
//! passed meanwhile is stopped at the first step after the adoption.
//!
//! # Labels, adoption and the lost run
//!
//! What a dead process leaves is files, supervisors and containers, and the runtime keeps a
//! ledger of them: before a lease's image is pinned ([`Oci::pin_image`]) or its first
//! directory is made, a sidecar file in `<scratch>/oci-labels` records the agent's id, the
//! run id, the lease's name, the container's labels and what the run booked (see
//! [`SweepReport`] and the module `ledger`); it is rewritten once the container started,
//! with its start and its supervisor, and removed after the last of the lease is.
//! [`CrunRuntime::open`] goes over the sidecars that name this agent: it adopts the run of
//! each whose record holds and whose container still stands (or ended while no agent ran,
//! and is reported from its exit record), and removes every other lease (the container's
//! state, the cgroup, the directories, then the sidecar), saying why; it also removes a
//! lease of this agent's name that no sidecar records. The module `adopt` has the checks, and
//! [`CrunRuntime::swept`] the [`SweepReport`] of what was done. A sidecar of another agent,
//! or one that is not well formed, is left alone and reported, so two agents on one host
//! never remove each other's leases.
//!
//! A pass that cannot remove a lease keeps its sidecar, and the next start tries again.
//! A single leftover is removed by its run id with [`CrunRuntime::remove_orphan`] (and
//! [`Runtime::remove`] of a handle the runtime does not hold does the same).
//!
//! A removed lease is a lost run: the restarted agent lists no container for it, so
//! [`Lifecycle::adopt`](crate::lifecycle::Lifecycle::adopt) holds nothing for it, the
//! agent's first beat lists no run, and the lease server gives up the lease the dead process
//! held (at once when the new process names its predecessor stopped, after the handover
//! grace otherwise) and runs the work again from the farm's rerun budget. An adopted run is
//! listed, and its lease is kept.
//!
//! # Images
//!
//! [`Runtime::prepare`] pulls and unpacks in the background ([`Prefetcher`]); a run's image
//! must be pinned by a sha256 digest. [`Runtime::create_and_start`] asks the store again
//! (one `stat` for an image that is there, a pull otherwise: single flight with the
//! prefetcher's), and reads the image's config from the file the image source kept
//! ([`RegistryImages::keeping_configs`](crate::image::RegistryImages::keeping_configs),
//! [`config_dir`]). With an image budget ([`CrunSettings::image_budget`]) both pulls count
//! the image against it and evict the images used least recently to fit; a run's lease
//! holds its image against eviction until the run is removed (the module `budget`).
//!
//! # Stop, kill and the end
//!
//! A stop sends SIGTERM (`crun kill`), waits for the run to end for the grace, and then
//! writes the lease cgroup's `cgroup.kill` (every process in it, the container's cgroup
//! among them) and waits for the end; a run that outlives that is an error. A container's
//! end is read once: the exit code of `crun run`, and whether the kernel's OOM killer ran
//! for the lease's own limit (`memory.events`, as js-contain classifies it): only then is
//! [`Exit::oom_killed`](crate::lifecycle::Exit::oom_killed) set.
//!
//! A container that did not run to an exit (`crun` or the init reported an error) polls as
//! exited with code 126, and the words they gave end its standard error tail.
//!
//! # Logs
//!
//! `crun run`'s output is captured by the supervisor with js-contain: two capped files in
//! the lease and a ring of the last bytes of each stream, whatever the job prints and
//! however slowly anything reads it. The ring reaches the agent in the exit record.
//! [`Runtime::log_tail`] gives the ring of an ended container as text (each stream at most
//! the wire's limit, `truncated` when the job wrote more than that), and keeps it with the
//! end, so it can be read again. A container never started, or still running, has an empty
//! tail.
//!
//! A runtime given [`LiveLogs`] ([`CrunRuntime::with_live_logs`]) uploads the output of each
//! run whose start carried a sink ([`RunSpec::log_sink`](crate::lifecycle::RunSpec)): the
//! run's capture files are capped at `--log-file-cap` (other runs keep the capture's 1 MiB),
//! an uploader ([`crate::logs`]) tails them from the start on its own thread, and the read of
//! the end tells it the totals without waiting. [`Runtime::logs_report`] gives the totals and
//! the bytes the capture dropped. The uploader keeps a progress record (`logs-upload.json`, no
//! secret) in the lease directory, and writes it no more once [`Runtime::remove`] removes the
//! directory. A run adopted after a restart whose lease holds one is given an uploader made
//! from it with the runtime's live logs, and one that ended meanwhile gets its finals and
//! reports its `logs` (the module `adopt`); a run adopted without one uploads nothing.
//!
//! # GPUs
//!
//! A runtime opened with a device list ([`CrunSettings::gpus`], from `--gpu-device`) runs GPU
//! jobs: a run that booked GPU indices is given their nodes, a device cgroup that allows only
//! those, and the listed driver mounts and variables (the module `gpu`). Without one,
//! `supports_gpus` is false and a start naming a GPU is refused.
//!
//! # Network
//!
//! A `none` job has loopback only. Under `--egress allow-list` or `open` the runtime checks the
//! firewall ([`EgressPort`]) before every wired create and wires a job that asked for a network
//! after `Oci::start_supervised` (its init holds it until the route), with a forward from the host
//! for each port it declared, releasing it when the run ends or goes. The wiring is the kernel's
//! and outlives the process: a runtime that is dropped leaves the wiring of every run it still
//! holds as it is ([`Wired::leave`]), as it leaves the runs, and the next process's runtime
//! takes over the wiring of each run it adopts from the run's record in the egress store
//! (the directory `egress` of the state root; [`EgressPort::adopt`], see the module `adopt`)
//! and has the egress remove the rest, records included ([`EgressPort::settle`]). A run that ended while no
//! agent ran has no network left, and is taken over so that its result is reported.
//!
//! A run's host ports ([`RunSpec::host_ports`], allowed by [`CrunSettings::host_ports`]) are
//! offered at every level, after `Oci::start_supervised`: a listener at each job port in the
//! container's network namespace, each connection joined to the host port on the host's
//! loopback ([`crate::host_ports`]), while the init holds the job until each listens. They are
//! closed when the run ends or goes, and recorded in the label `label::HOST_PORTS`, from which
//! an adopted run that still stands is offered them again; one that cannot be is answered by
//! [`Runtime::host_port_lost`], and the lifecycle stops it.
//!
//! # Host jobs
//!
//! A runtime opened with [`CrunSettings::host_jobs`] (`--allow-host-jobs`) launches a run
//! whose isolation is host as an OCI container that shares the host: js-contain's host config
//! (`Isolation::Host`), from the same spec as a contained run's with the namespaces, the
//! root and the privileges the only differences. It runs as root in the host's pid, network,
//! mount, IPC, UTS and cgroup namespaces (no user namespace), with the host's `/` as its root,
//! every capability and no seccomp filter, in the run's scratch directory; the image's config
//! still gives its program and environment. Its cgroup is `ctr` below the lease's, which the
//! booking limits, so it is accounted as a contained run is, and a stop's or a removal's write
//! to the lease's `cgroup.kill` ends it whole. It has no PID namespace whose end takes its
//! other processes with its first, so its init kills what its program left once the program
//! has ended (`js-init --end-descendants`): the run ends when its program does. Its lease directory and cgroup are the agent's
//! own, the bundle writes a host-lease marker of the agent's into the directory, and `crun`
//! and its supervisor run as the agent for a lease whose directory and marker are both the
//! agent's (`Oci::lease_user`): every other lease's are the job user's, whoever made its
//! directory. It is not wired (it has the host's network), and a
//! host run that names a mount, run files or a GPU is refused, as is every host run of a
//! runtime opened without host jobs.
//!
//! # Layers and mounts
//!
//! A runtime opened with [`CrunSettings::fs`] (where the kernel has `openat2`, `open_tree`,
//! `move_mount` and `mount_setattr`) gives a job start its layers and mounts of host paths
//! (`docs/design/job-rootfs.md`, the module `fs`): while the start waits, each host path is
//! opened beneath its `--mount-root` on the `js-lookups` pool ([`Runtime::resolve_fs`]); then
//! the images are pinned for the lease (an image layer as `<lease>.l<k>`) and the assembly
//! clones the paths (on the `js-paths` pool, which the checks after a job use too, so no
//! lookup waits behind them) and mounts the layered root and the copy-on-write mounts at
//! `/run/js-agent/mounts/<lease>`, listing each mount before it is made
//! ([`Runtime::assemble_fs`]). The container is made over the assembled root, writable, with
//! the clones bound where the start named them. Every end of the run (its removal, a failed
//! make, the sweep of an earlier process's leases, a run never made) unmounts from that list,
//! deepest first, before the lease directory, which holds the writable layers, is removed,
//! and removes the pins. A job that did not succeed is held as running until each of its
//! sources has answered a check ([`Runtime::input_failure`]). A run start's mount of a named
//! root is refused: no path is bound by its text.
//!
//! # Not here
//!
//! No read-back of the container's mounts: none is bound by a path a job could change.

mod adopt;
#[cfg(test)]
mod adopt_tests;
mod budget;
#[cfg(test)]
mod budget_tests;
mod claim;
mod create;
mod dirs;
mod fault;
pub mod fs;
mod gpu;
mod ledger;
#[cfg(test)]
mod ledger_tests;
mod make;
mod net;
#[cfg(test)]
mod net_tests;
mod peak;
mod pull;
mod remove;
mod rootfs;
mod settings;
mod spec;
#[cfg(test)]
mod tests;
mod whereabouts;
#[cfg(test)]
mod whereabouts_tests;
mod wire;

pub use claim::{ClaimError, HostClaim, prepare_store, require_trusted_ancestors};
pub use dirs::hand_over_no_follow;
use dirs::{cgroup_dir, legacy_lease, make_helper_cgroup};
use fault::Fault;
pub use gpu::{
    DESTINATION_OFF_LIMITS, DeviceProblem, EnvProblem, GpuFlagError, GpuPassthrough, GpuSettings,
    INDEX, MountProblem, NO_GPUS, NodeError, ReadNode, read_node,
};
pub(crate) use net::port_of;
pub use net::{EgressPort, Wired, start_egress};
#[cfg(test)]
pub(crate) use rootfs::warn_unreadable;
pub use settings::{CgroupSettings, CrunError, CrunSettings, Hooks, HostPorts};
pub use wire::{
    CgroupError, CgroupSource, ID_TOOL_PATH, IdSources, MountSeam, PULL_THREADS, Seams, WireError,
    oci_config, open as open_for_plan,
};

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{JobUsage, LogReport, LogTail, NetworkCeiling, PortRange};
use js_contain::cgroup::{LeaseCgroup, ROOT_OWNED_FILES};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::ImageSource;
use js_contain::oci::{GcReport, LeaseDirs, Oci, Supervised};
use tokio::runtime::{Builder, Runtime as TokioRuntime};

use crate::cli_runtime::container_name;
use crate::host_ports::Forwarder;
use crate::lifecycle::{
    Created, Exit, FilesRoot, Found, FsPrepared, JobFs, Prefetcher, Prepared, Removal, RunHandle,
    RunSpec, Runtime, RuntimeError, Status, Ticket, Wake, Whereabouts,
};
use crate::logs::{LiveLogs, RunLogs};
use ledger::{Entry, Ledger};
pub use ledger::{Failed, LedgerError, Left, Reason, Removed, SweepReport};
use pull::Images;
use spec::{concluded, ctr_of, log_tail_of, usage_of};

/// How long a pull and unpack of one image may take before it counts as failed.
pub const PULL_LIMIT: Duration = Duration::from_secs(30 * 60);

/// How long a `crun kill` may take before it is given up: a hung `crun` must not stall the
/// single worker. The stop goes on to the lease cgroup's kill.
pub const CALL_LIMIT: Duration = Duration::from_secs(120);

/// How long a run is waited for after its cgroup was killed. A process in an
/// uninterruptible sleep outlasts it; the stop then fails and is tried again.
pub const KILL_WAIT: Duration = Duration::from_secs(60);

/// The least a removal waits for a supervisor it ended by force: a process that was sent
/// SIGKILL is a zombie a moment later, so a wait of none would fail a removal that works.
const FORCE_FLOOR: Duration = Duration::from_millis(500);

/// The directory of the image configs that the image source keeps and the runtime reads,
/// for a runtime whose store is `scratch`.
#[must_use]
pub fn config_dir(scratch: &Path) -> PathBuf {
    scratch.join("oci-configs")
}

/// How far a container is.
enum State {
    /// Made and not started.
    Created,
    /// The supervisor of `crun run` runs (or has ended and its end has not been read); the
    /// container started at `since_ms`, wall clock.
    Running { run: Supervised, since_ms: u64 },
    /// Ended, as read once: the exit, and the tail of what the run wrote.
    Ended(Exit, LogTail),
}

/// One container.
struct Run {
    spec: ContainerSpec,
    labels: BTreeMap<String, String>,
    lease: LeaseDirs,
    cgroup: LeaseCgroup,
    /// The OOM count of the cgroup above, when the lease started.
    backstop: Option<u64>,
    state: State,
    /// The container's network: its level, and its wiring from its start until released.
    net: net::Net,
    /// The lease's sidecar as last written.
    entry: Entry,
    /// Its live logs: the sink, then the uploader and the report.
    logs: RunLogs,
    /// What its file system cloned, for the check after it ends; none without host paths.
    fs: Option<rootfs::RunFs>,
    /// The steps of its start, until [`Runtime::start_steps`] takes them.
    marks: make::Marks,
    /// Its CPU over time, for its peak: sampled from its start (or, for an adopted run, from
    /// its first poll) each time it is polled ([`peak::PeakWindow`]).
    peak: peak::PeakWindow,
}

/// A container runtime over `crun`. Dropping it leaves its runs as they are, their network
/// wiring included, for the next process of the agent to adopt.
pub struct CrunRuntime {
    oci: Arc<Oci>,
    /// The image store as the pulls and the leases use it, within the image budget.
    images: Arc<Images>,
    pulls: Prefetcher,
    /// The runtime of the calls that wait, shared with the create pool's threads.
    rt: Arc<TokioRuntime>,
    cgroups: CgroupSettings,
    host_id: String,
    configs: PathBuf,
    files: Option<FilesRoot>,
    gpus: Option<GpuSettings>,
    /// The operator's CPU ceiling ([`CrunSettings::cpu_ceiling`]).
    cpu_ceiling: Option<js_contain::CpuCeiling>,
    /// Whether host jobs are launched ([`CrunSettings::host_jobs`]).
    host_jobs: bool,
    /// Whether the leases' directories are made beside the runs' own
    /// ([`OciConfig::leases_in`](js_contain::OciConfig::leases_in)), where [`Runtime::lease_dir`] names them.
    leases_beside: bool,
    /// The image store, where an agent from before the leases were beside the runs kept them.
    store: PathBuf,
    /// The host's egress; none in mode none.
    egress: Option<Arc<dyn EgressPort>>,
    /// The host ports a job may reach, and the forwarder that offers them.
    host_ports: (Vec<PortRange>, Arc<Forwarder>),
    hooks: Hooks,
    runs: BTreeMap<String, Run>,
    ledger: Ledger,
    swept: SweepReport,
    /// Live logs, when the runtime uploads them ([`CrunRuntime::with_live_logs`]).
    logs: Option<LiveLogs>,
    /// What jobs' layers and mounts are made with; none for a runtime that gives none.
    fs: Option<fs::Fs>,
    /// The makes and starts of leases under way off the worker.
    creates: create::Creates,
    /// The removals of leases under way off the worker.
    removals: remove::Removals,
}

impl std::fmt::Debug for CrunRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrunRuntime")
            .field("host_id", &self.host_id)
            .field("containers", &self.runs.len())
            .finish_non_exhaustive()
    }
}

impl Drop for CrunRuntime {
    fn drop(&mut self) {
        // The runs outlive the runtime (their supervisors and containers are not this
        // process's), and so does their wiring: an `Attachment` dropped would take it down
        // and cut a run the next process adopts.
        for run in self.runs.values_mut() {
            run.net.leave();
        }
    }
}

impl CrunRuntime {
    /// Opens the runtime over the store in `settings.scratch`, fetching images with
    /// `source`.
    ///
    /// # Errors
    /// See [`CrunError`].
    pub fn open(settings: CrunSettings, source: Arc<dyn ImageSource>) -> Result<Self, CrunError> {
        Self::open_with(settings, source, Hooks::default())
    }

    /// [`CrunRuntime::open`] with the hooks given.
    ///
    /// # Errors
    /// See [`CrunError`].
    pub fn open_with(
        settings: CrunSettings,
        source: Arc<dyn ImageSource>,
        hooks: Hooks,
    ) -> Result<Self, CrunError> {
        let (uid, gid) = settings.oci.job_user.ok_or(CrunError::NoJobUser)?;
        let cgroups = settings.cgroups;
        // The post-job checks know the leases' group by its name: a leftover job process
        // under another name would be judged the host's, and the check silently off.
        if Path::new(&cgroups.actions).file_name() != Some(OsStr::new("actions")) {
            return Err(CrunError::ActionsName {
                actions: cgroups.actions,
            });
        }
        // `crun`, run as the job user, moves its first process from the agent's cgroup into
        // the container's, which needs write access to the `cgroup.procs` of their nearest
        // common ancestor: the agent's own cgroup.
        for file in ROOT_OWNED_FILES {
            let path = cgroup_dir(&cgroups.mount, &cgroups.root).join(file);
            (hooks.chown)(&path, uid, gid).map_err(|why| CrunError::Delegation { path, why })?;
        }
        // The helper containers (unpack, removal) are made by `crun` as the job user in a
        // cgroup of their own, so that the job user is given nothing of the directory the
        // leases' cgroups are in (it could make or remove the cgroups of other leases
        // there). A lease's cgroup is given to the job user when it is made. It exists
        // before the backend does: the backend sweeps old unpacks with a helper when it
        // starts.
        let helpers = format!("{}/helpers", cgroups.actions.trim_end_matches('/'));
        let helpers_dir = cgroup_dir(&cgroups.mount, &helpers);
        make_helper_cgroup(&helpers_dir, hooks.chown, (uid, gid)).map_err(|why| {
            CrunError::Helpers {
                path: helpers_dir.clone(),
                why,
            }
        })?;
        let leases_beside = settings.oci.leases_in.is_some();
        let oci = Oci::new(&settings.oci, source, &settings.scratch, &helpers)?;
        let rt = Arc::new(
            Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        );
        let configs = config_dir(&settings.scratch);
        std::fs::create_dir_all(&configs).map_err(|why| CrunError::Configs {
            path: configs.clone(),
            why,
        })?;
        let ledger = Ledger::open(&settings.scratch, &settings.host_id)?;
        let images = Images::of(
            &oci,
            &settings.scratch,
            hooks.pull_limit,
            settings.image_budget,
        );
        let pulling = Arc::clone(&images);
        let pulls = Prefetcher::new(settings.pull_threads, move |image| pulling.pull(image))?;
        let forgetter = pulls.forgetter();
        images.on_evict(Box::new(move |digest| forgetter.forget_digest(digest)));
        let mut runtime = Self {
            oci,
            images,
            pulls,
            rt,
            cgroups,
            host_id: settings.host_id,
            configs,
            files: settings.files,
            gpus: settings.gpus,
            host_jobs: settings.host_jobs,
            cpu_ceiling: settings.cpu_ceiling,
            leases_beside,
            store: settings.scratch.clone(),
            egress: settings.egress,
            host_ports: (
                settings.host_ports.allowed,
                Arc::new(Forwarder::new(settings.host_ports.listen)),
            ),
            hooks,
            runs: BTreeMap::new(),
            ledger,
            swept: SweepReport::default(),
            logs: None,
            fs: settings.fs.map(fs::Fs::open).transpose()?,
            creates: create::Creates::new()?,
            removals: remove::Removals::new()?,
        };
        // No container of this process exists yet: the runs of an earlier process of this
        // agent that still stand are adopted, and the rest of its leases are removed.
        runtime.swept = runtime.recover()?;
        Ok(runtime)
    }

    /// This runtime uploading the output of each run whose start carries a sink, with `logs`;
    /// each run it adopted whose lease holds a progress record is given its uploader now
    /// (the module `adopt`). Given once, after the runtime opened.
    #[must_use]
    pub fn with_live_logs(mut self, logs: LiveLogs) -> Self {
        self.resume_uploads(&logs);
        self.logs = Some(logs);
        self
    }

    /// What the pass over the ledger at [`CrunRuntime::open`] did: the runs adopted and the
    /// leases removed.
    #[must_use]
    pub fn swept(&self) -> &SweepReport {
        &self.swept
    }

    /// [`Runtime::prepare`], waiting for the fetch to end: `Ready` or `Failed`. For a
    /// command that pulls one image.
    #[must_use]
    pub fn wait_prepared(&self, image: &str) -> Prepared {
        self.pulls.wait(image)
    }

    /// Forgets what is known of the pull of `image` ([`Prefetcher::forget`]): the owner of
    /// an image store that removes or evicts an image must call it, or the image is still
    /// answered `Ready` after it is gone. The next [`Runtime::prepare`] pulls it again.
    pub fn forget(&self, image: &str) {
        self.pulls.forget(image);
    }

    /// Removes the image digests the store holds that nothing names: not one of
    /// `configured` (the digests the executor is set up with: its base and tool images,
    /// `sha256:<hex>`) and not the image of a container this runtime, or another agent's
    /// runtime on the same store, holds: staged, running or ended and not yet removed
    /// (each such container pins its image, [`Oci::pin_image`]). The digests it removes are
    /// forgotten by this runtime's prefetcher, so none is answered `Ready` after it is gone
    /// (another agent's prefetcher may still say `Ready`: its next run fetches the image
    /// again within the pull limit). For the start, after the leases a dead agent left have
    /// been swept and before the disk to offer is read; see [`Oci::collect_images`] for what
    /// is kept and why it is safe beside a pull.
    ///
    /// The kept image configs of removed digests stay (a pull of the digest writes it
    /// again).
    ///
    /// # Errors
    /// The image store cannot be read; a digest that could not be removed is in the
    /// report.
    pub fn collect_images(&self, configured: &[String]) -> Result<GcReport, String> {
        let leased: Vec<String> = self
            .runs
            .values()
            .map(|run| run.spec.image_digest.clone())
            .collect();
        let report = self.oci.collect_images(configured, &leased)?;
        for digest in &report.removed {
            self.pulls.forget_digest(digest);
        }
        Ok(report)
    }

    /// Waits until the container `id` has exited (one that is gone, never started or ended
    /// already returns at once). The lifecycle polls instead; this is for a caller with
    /// nothing else to do.
    pub fn wait_ended(&mut self, id: &RunHandle) {
        let rt = &self.rt;
        if let Some(Run {
            state: State::Running { run, .. },
            ..
        }) = self.runs.get_mut(&id.0)
        {
            rt.block_on(run.wait());
        }
    }

    /// Reads how the container `name` stands, ending it in the books once its supervisor wrote
    /// the end, and holding a run that ended as running until its sources were checked
    /// ([`CrunRuntime::checked`]).
    fn status_of(&mut self, name: &str) -> Status {
        match self.read_status(name) {
            Status::Exited(exit) => self.checked(name, exit),
            other => other,
        }
    }

    /// [`CrunRuntime::status_of`] before the check of the run's sources.
    fn read_status(&mut self, name: &str) -> Status {
        let (rt, oci) = (&self.rt, &self.oci);
        let Some(run) = self.runs.get_mut(name) else {
            return Status::Gone;
        };
        let (ended, captured) = match &run.state {
            State::Created => return Status::Running,
            State::Ended(exit, _) => return Status::Exited(*exit),
            State::Running { run: running, .. } => match running.try_end() {
                None => {
                    let used = std::fs::read_to_string(run.cgroup.dir().join("cpu.stat"));
                    let used = used
                        .ok()
                        .as_deref()
                        .and_then(crate::usage::parse_usage_usec);
                    if let Some(used) = used {
                        run.peak.observe((self.hooks.mono_us)(), used);
                    }
                    return Status::Running;
                }
                // The drain of the streams is over once the supervisor wrote its record:
                // what it kept is all the run wrote.
                Some(end) => (rt.block_on(oci.ended_as(&run.spec, end)), running.tail()),
            },
        };
        let (exit, tail) = concluded(ended, &captured, (&run.cgroup, run.backstop));
        run.logs.end((captured.stdout.total, captured.stderr.total));
        run.state = State::Ended(exit, tail);
        run.net.release(name);
        Status::Exited(exit)
    }
}

impl Runtime for CrunRuntime {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.pulls.prepare(image, waited)
    }

    fn set_wake(&mut self, wake: Wake) {
        self.pulls.set_wake(wake.clone());
        self.creates.set_wake(wake.clone());
        self.removals.set_wake(wake.clone());
        self.fs.iter_mut().for_each(|fs| fs.set_wake(wake.clone()));
    }

    fn start_steps(&mut self, id: &RunHandle) -> Vec<(&'static str, u64)> {
        let run = self.runs.get_mut(&id.0);
        run.map(|run| std::mem::take(&mut run.marks))
            .unwrap_or_default()
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let id = self.stage(spec)?;
        if let Err(why) = self.start_staged(&id) {
            // The failed start leaves no container: the lease is taken apart as a removal
            // does. What cannot be is kept, listed, and removed by the next removal or sweep.
            if let Err(left) = self.remove(&id) {
                tracing::warn!(lease = %id.0, "remove the failed start: {left}");
            }
            return Err(why);
        }
        Ok(id)
    }

    fn begin_create(&mut self, spec: &RunSpec) -> Created {
        self.begin_off_worker(spec)
    }

    fn poll_create(&mut self, run_id: &str) -> Created {
        self.poll_off_worker(run_id)
    }

    fn creating(&self, run_id: &str) -> Option<RunHandle> {
        self.creating_off_worker(run_id)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        Ok(self.status_of(&id.0))
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        let (rt, oci, kill_wait) = (&self.rt, &self.oci, self.hooks.kill_wait);
        let call_limit = self.hooks.call_limit;
        let Some(Run {
            state: State::Running { run, .. },
            cgroup,
            spec,
            ..
        }) = self.runs.get_mut(&id.0)
        else {
            // Gone, never started or ended: nothing runs.
            return Ok(());
        };
        if run.try_end().is_some() {
            return Ok(());
        }
        // A SIGTERM that fails (the container is not made yet, or already over) is not
        // the stop's failure: the wait below ends at once for a container that is over,
        // and the kill does the rest for one that is not.
        // Nor is a `crun kill` that does not answer within the call limit (it is dropped,
        // which kills it): the cgroup kill below does the rest.
        let _ = rt
            .block_on(async { tokio::time::timeout(call_limit, oci.terminate(&spec.name)).await });
        if rt
            .block_on(async { tokio::time::timeout(grace, run.wait()).await })
            .is_ok()
        {
            return Ok(());
        }
        cgroup
            .kill()
            .map_err(|why| RuntimeError::new(format!("kill the lease's cgroup: {why}")))?;
        rt.block_on(async { tokio::time::timeout(kill_wait, run.wait()).await })
            .map_err(|_| RuntimeError::new(format!("{} still runs after its kill", spec.name)))
    }

    /// Removes the container and everything its lease made on the caller's thread (the
    /// module `remove`). A handle this process does not hold is a leftover of an earlier
    /// process: it is removed if the ledger holds it as this agent's, nothing happens if the
    /// ledger does not know it, and the call fails (and changes nothing) if its sidecar is
    /// another agent's or is not well formed, or a create or a removal of it is under way.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.remove_now(id)
    }

    /// On a thread of the removal pool (the module `remove`).
    fn removing(&mut self, id: &RunHandle) -> Removal {
        self.remove_off_worker(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        let now = (self.hooks.now_ms)();
        Ok(self
            .runs
            .iter()
            .map(|(name, run)| Found {
                id: RunHandle(name.clone()),
                labels: run.labels.clone(),
                running_ms: match &run.state {
                    State::Running { since_ms, .. } => now.saturating_sub(*since_ms),
                    State::Created | State::Ended(..) => 0,
                },
                never_started: matches!(run.state, State::Created),
            })
            .collect())
    }

    fn supports_gpus(&self) -> bool {
        self.gpus.is_some()
    }

    fn check_egress(&self) -> Result<(), RuntimeError> {
        net::check(self.egress.as_deref())
    }

    fn network_ceiling(&self) -> NetworkCeiling {
        net::ceiling(self.egress.as_deref())
    }

    fn host_ports(&self) -> Vec<PortRange> {
        self.host_ports.0.clone()
    }

    fn host_port_lost(&self, id: &RunHandle) -> Option<String> {
        self.runs.get(&id.0)?.net.lost()
    }

    fn layered_rootfs(&self) -> bool {
        self.gives_layers()
    }

    fn check_layer_image(&self, image: &str) -> Result<(), String> {
        self.check_layer(image)
    }

    /// None: a job start's host paths are opened beneath their roots, never bound by path.
    fn binds_named_mounts(&self) -> bool {
        false
    }

    fn resolve_fs(&mut self, run_id: &str, image: &str, fs: &JobFs, now_ms: u64) -> FsPrepared {
        self.resolve(run_id, image, fs, now_ms)
    }

    fn assemble_fs(&mut self, run_id: &str, now_ms: u64) -> FsPrepared {
        self.assemble(run_id, now_ms)
    }

    fn discard_fs(&mut self, run_id: &str) -> Result<(), RuntimeError> {
        self.discard(run_id)
    }

    fn mounts_of(&self, run_id: &str) -> Option<PathBuf> {
        let name = container_name(&self.host_id, run_id).ok()?;
        Some(self.fs.as_ref()?.dir_of(&name))
    }

    fn input_failure(&self, id: &RunHandle) -> Option<String> {
        self.unreadable(&id.0)
    }

    fn ends_held(&self, id: &RunHandle) -> bool {
        self.held(&id.0)
    }

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        if !self.leases_beside {
            return None;
        }
        let name = container_name(&self.host_id, run_id).ok()?;
        Some(self.oci.lease_dirs(&name).dir)
    }

    fn tmp_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.tmp_dir_of_run(run_id)
    }

    fn places_in_cgroup(&self) -> bool {
        true
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        Ok(self.runs.get(&id.0).map(|run| match &run.state {
            State::Created => LogTail::default(),
            // Read before the end was read: the exit record's tail, empty until there is one.
            State::Running { run, .. } => log_tail_of(&run.tail(), None),
            State::Ended(_, tail) => tail.clone(),
        }))
    }

    fn logs_report(&mut self, id: &RunHandle) -> Option<LogReport> {
        self.runs.get(&id.0).and_then(|run| run.logs.report())
    }

    fn usage(&mut self, id: &RunHandle) -> Option<JobUsage> {
        // The lease's group, so that everything the job ran in it is counted; it is removed
        // with the container, after this is read. Its peak ends with this reading.
        let now = (self.hooks.mono_us)();
        let run = self.runs.get(&id.0)?;
        let usage = usage_of(&run.cgroup)?;
        let peak = run.peak.clone().finish(now, usage.cpu_usage_usec);
        Some(JobUsage {
            cpu_peak: peak,
            ..usage
        })
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        let run = self.runs.get(&id.0)?;
        let ctr = ctr_of(run.cgroup.dir());
        matches!(run.state, State::Running { .. })
            .then_some(ctr)
            .filter(|ctr| ctr.is_dir())
    }

    fn whereabouts(&mut self, id: &RunHandle) -> Whereabouts {
        let running = self.runs.get(&id.0);
        match running.filter(|run| matches!(run.state, State::Running { .. })) {
            Some(run) => whereabouts::whereabouts_of(
                &ctr_of(run.cgroup.dir()),
                self.oci.container_pid(&run.spec),
                self.hooks.proc_read,
            ),
            None => Whereabouts::NoProcess,
        }
    }
}
