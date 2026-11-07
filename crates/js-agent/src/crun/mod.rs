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
//! - the cgroup `<actions>/<name>`, made by the agent with the booking as the limit
//!   (memory the booking with no swap, CPU the booked cores: the arithmetic the lease
//!   server's booking assumes) and given to the job user, who makes the container's cgroup
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
//! - a bundle of the image's unpacked root (read-only), an overlay of an empty input root
//!   at `/work`, where the job starts, the image config's program with the run's arguments
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
//! firewall ([`EgressPort`]) before every create and wires a job that asked for a network after
//! `Oci::start_supervised` (its init holds it until the route), with a forward from the host
//! for each port it declared, releasing it when the run ends or goes. A runtime with such an
//! egress adopts no run that may still run: the egress removes the wiring of a previous process
//! when it starts, so such a lease an earlier agent left is removed. A run that ended while no
//! agent ran has no network left, and is taken over so that its result is reported.
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
//! to the lease's `cgroup.kill` ends it whole. Its lease directory and cgroup are the agent's
//! own, the bundle writes a host-lease marker of the agent's into the directory, and `crun`
//! and its supervisor run as the agent for a lease whose directory and marker are both the
//! agent's (`Oci::lease_user`): every other lease's are the job user's, whoever made its
//! directory. It is not wired (it has the host's network), and a
//! host run that names a mount, run files or a GPU is refused, as is every host run of a
//! runtime opened without host jobs.
//!
//! # Not here
//!
//! No read-back of the container's mounts.

mod adopt;
#[cfg(test)]
mod adopt_tests;
mod budget;
#[cfg(test)]
mod budget_tests;
mod claim;
mod dirs;
mod fault;
mod gpu;
mod ledger;
#[cfg(test)]
mod ledger_tests;
mod net;
mod pull;
mod settings;
mod spec;
#[cfg(test)]
mod tests;
mod wire;

pub use dirs::hand_over_no_follow;
use dirs::{cgroup_dir, legacy_lease, make_dir, make_helper_cgroup, owned, project_of, take_dir};
use fault::Fault;
pub(crate) use net::port_of;
pub use net::{EgressPort, Wired, start_egress};
pub use settings::{CgroupSettings, CrunError, CrunSettings, Hooks};

pub use claim::{ClaimError, HostClaim, prepare_store, require_trusted_ancestors};
pub use gpu::{
    DESTINATION_OFF_LIMITS, DeviceProblem, EnvProblem, GpuFlagError, GpuPassthrough, GpuSettings,
    INDEX, MountProblem, NO_GPUS, NodeError, ReadNode, read_node,
};

pub use wire::{
    CgroupError, CgroupSource, ID_TOOL_PATH, IdSources, PULL_THREADS, Seams, WireError, oci_config,
    open as open_for_plan,
};

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{LogReport, LogTail, NetworkCeiling};
use js_contain::cgroup::{JOB_OWNED_FILES, LeaseCgroup, LeasePolicy, ROOT_OWNED_FILES};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::{ImageSource, PinnedImage};
use js_contain::oci::{GcReport, LeaseDirs, Oci, Owner, Supervised, stderr_path, stdout_path};
use js_types::JobNetwork;
use js_types::Resources;
use tokio::runtime::{Builder, Runtime as TokioRuntime};

use crate::cli_runtime::container_name;
use crate::image::{ImageConfig, config_path};
use crate::lifecycle::{
    Body, Exit, FilesRoot, Found, Prefetcher, Prepared, RunHandle, RunSpec, Runtime, RuntimeError,
    Status, Ticket, check_run_env, label,
};
use crate::logs::{LiveLogs, RunLogs};
use ledger::{Entry, Ledger, Supervisor};
pub use ledger::{Failed, LedgerError, Left, Reason, Removed, SweepReport};
use pull::Images;
use spec::{concluded, container_spec, ctr_of, host_spec, job_env, log_tail_of, pinned};

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
    /// The lease's `memory.max`.
    cap: Option<u64>,
    /// The OOM count of the cgroup above, when the lease started.
    backstop: Option<u64>,
    state: State,
    /// The container's network: its level, and its wiring from its start until released.
    net: net::Net,
    /// The lease's sidecar as last written.
    entry: Entry,
    /// Its live logs: the sink, then the uploader and the report.
    logs: RunLogs,
}

/// A container runtime over `crun`.
pub struct CrunRuntime {
    oci: Arc<Oci>,
    /// The image store as the pulls and the leases use it, within the image budget.
    images: Arc<Images>,
    pulls: Prefetcher,
    rt: TokioRuntime,
    cgroups: CgroupSettings,
    host_id: String,
    configs: PathBuf,
    files: Option<FilesRoot>,
    gpus: Option<GpuSettings>,
    /// Whether host jobs are launched ([`CrunSettings::host_jobs`]).
    host_jobs: bool,
    /// Whether the leases' directories are made beside the runs' own
    /// ([`OciConfig::leases_in`](js_contain::OciConfig::leases_in)), where [`Runtime::lease_dir`] names them.
    leases_beside: bool,
    /// The image store, where an agent from before the leases were beside the runs kept them.
    store: PathBuf,
    /// The host's egress; none in mode none.
    egress: Option<Arc<dyn EgressPort>>,
    hooks: Hooks,
    runs: BTreeMap<String, Run>,
    ledger: Ledger,
    swept: SweepReport,
    /// Live logs, when the runtime uploads them ([`CrunRuntime::with_live_logs`]).
    logs: Option<LiveLogs>,
}

impl std::fmt::Debug for CrunRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrunRuntime")
            .field("host_id", &self.host_id)
            .field("containers", &self.runs.len())
            .finish_non_exhaustive()
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
        let rt = Builder::new_current_thread().enable_all().build()?;
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
            leases_beside,
            store: settings.scratch.clone(),
            egress: settings.egress,
            hooks,
            runs: BTreeMap::new(),
            ledger,
            swept: SweepReport::default(),
            logs: None,
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

    /// Makes the container of `spec` and does not start it: the first half of
    /// [`Runtime::create_and_start`], for a caller that starts it later (or never, as the
    /// conformance suite does). Such a container lists as
    /// [`never_started`](Found::never_started).
    ///
    /// # Errors
    /// As [`Runtime::create_and_start`], and an environment [`check_run_env`] refuses.
    pub fn stage(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        check_run_env(&spec.env)?;
        self.make(spec)
            .map(RunHandle)
            .map_err(|fault| RuntimeError::new(fault.to_string()))
    }

    /// Makes the container of `spec`, everything but its start. A failure removes what it
    /// made.
    ///
    /// An image in the store with no kept config is removed and fetched again. That deletes
    /// the root such an image gives as the lower layer of a container's mounts, so it relies
    /// on every container of an earlier daemon that still runs on it pinning it (an adopted
    /// run's pin stays until its removal, and a pinned image is not removed), and on the
    /// start-up pass that removes the rest of what an earlier agent left (P12b) having run
    /// before the first run is made. The removal and a fetch of the same image never overlap
    /// (the image store holds one gate for each digest).
    ///
    /// A host job is refused unless the runtime launches them ([`CrunSettings::host_jobs`]),
    /// and when it names a mount, run files or a GPU; it is not wired, since it has the
    /// host's network.
    fn make(&mut self, spec: &RunSpec) -> Result<String, Fault> {
        let host = spec.isolation == js_agent_wire::Isolation::Host;
        if host {
            if !self.host_jobs {
                return Err(Fault::NotContained);
            }
            if !spec.mounts.is_empty() || spec.files.is_some() {
                return Err(Fault::HostMounts);
            }
            if !spec.gpu_indices.is_empty() {
                return Err(Fault::HostGpus);
            }
        } else {
            net::port_for(self.egress.as_deref(), spec.network).map_err(Fault::Network)?;
        }
        // Before anything is made: a run's nodes are read now, and one that is gone refuses it.
        let gpu = gpu::wiring(self.gpus.as_ref(), &spec.gpu_indices).map_err(Fault::Gpu)?;
        let Body::Container { image: given, args } = &spec.body else {
            return Err(Fault::Native);
        };
        let image = pinned(given).map_err(Fault::Image)?;
        let name = container_name(&self.host_id, &spec.run_id)
            .map_err(|_| Fault::Name(spec.run_id.clone()))?;
        if self.runs.contains_key(&name) {
            return Err(Fault::InUse(spec.run_id.clone()));
        }
        let now = (self.hooks.now_ms)();
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
        // Before the first thing the sidecar names is made (see the module `ledger`), the
        // image's pin among them: the next start removes a pin a dead process left, once it
        // has removed the lease's container.
        let entry = self.entry_for(spec, &name, (&labels, &image.digest));
        self.ledger
            .record(&entry)
            .map_err(|why| Fault::step("record the lease's labels", &why))?;
        let (container, lease, cgroup) =
            match self.make_recorded(spec, (given, &image), args, &name, gpu) {
                Ok(made) => made,
                Err(fault) => {
                    // What the failed step left; none of it is a container. The sidecar goes with
                    // it (the pin last), or stays for the next start when some of it cannot be
                    // removed.
                    let _ = self.ledger.discard(&entry, |entry| self.tear_down(entry));
                    return Err(fault);
                }
            };
        self.runs.insert(
            name.clone(),
            Run {
                spec: container,
                labels,
                lease,
                cgroup,
                cap: js_contain::hard_memory_max(spec.memory_bytes),
                backstop: None,
                state: State::Created,
                net: match host {
                    // The host's own network: nothing to wire, nothing to forward.
                    true => net::Net::asked(JobNetwork::None, Vec::new()),
                    false => net::Net::asked(spec.network, spec.ports.clone()),
                },
                entry,
                logs: RunLogs::asked(self.logs.as_ref(), spec.log_sink.as_ref()),
            },
        );
        Ok(name)
    }

    /// The pinned image (`given`, parsed as `image`), the container spec with `args` and what
    /// its GPUs give it, and the resources of the lease `name`, whose sidecar is recorded.
    fn make_recorded(
        &self,
        spec: &RunSpec,
        (given, image): (&str, &PinnedImage),
        args: &[String],
        name: &str,
        gpu: gpu::Wiring,
    ) -> Result<(ContainerSpec, LeaseDirs, LeaseCgroup), Fault> {
        // Before the image is made sure of: from then on no agent on the store removes it
        // until the lease is torn down.
        self.oci.pin_image(image, name)?;
        self.images.lease(name, &image.digest);
        let ensure = || self.rt.block_on(self.images.ensure(image));
        ensure().map_err(Fault::Image)?;
        if !config_path(&self.configs, &image.digest).exists() {
            // An image an earlier daemon (or a source that keeps no configs) unpacked has
            // no config to run it by: it is fetched again, and the source keeps the config
            // this time. The prefetcher is told, as it is for any image taken out. A run of
            // another agent on the image pins it, and the removal is refused.
            self.oci.unpin_image(name)?;
            self.rt.block_on(self.oci.remove_image(image))?;
            self.pulls.forget(given);
            self.oci.pin_image(image, name)?;
            ensure().map_err(Fault::Image)?;
        }
        let rootfs = js_contain::oci::image::published_root(&self.store, &image.digest);
        gpu.check_targets(&rootfs).map_err(Fault::Gpu)?;
        let config = ImageConfig::kept(&self.configs, &image.digest)?;
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, name);
        let lease = self.oci.lease_dirs(name);
        let mut container = match spec.isolation {
            js_agent_wire::Isolation::Contained => {
                container_spec(name, spec, image, &lease, cgroup.name())
            }
            js_agent_wire::Isolation::Host => host_spec(name, spec, image, &lease, cgroup.name()),
        };
        config.apply(args, &mut container)?;
        job_env(&mut container, &spec.env);
        gpu.apply(&mut container);
        self.make_resources(spec, &container, &lease, &cgroup)?;
        Ok((container, lease, cgroup))
    }

    /// The directories, hand-overs, cgroup and bundle of one lease.
    ///
    /// A host lease ([`Isolation::Host`](js_contain::Isolation::Host)) is the agent's own:
    /// only its directory is made (no overlay, home or input root), and it, the run's scratch
    /// directory and the cgroup's files are given to the agent itself, not to the job user or
    /// the job's ids; its bundle adds the host-lease marker, so `crun` runs as the agent for
    /// it ([`Oci::lease_user`]). Its bundle is the loopback one: a host job is never wired.
    fn make_resources(
        &self,
        spec: &RunSpec,
        container: &ContainerSpec,
        lease: &LeaseDirs,
        cgroup: &LeaseCgroup,
    ) -> Result<(), Fault> {
        let chown = self.hooks.chown;
        let host = container.isolation == js_contain::Isolation::Host;
        let ((job, job_id), dirs, network) = match host {
            true => {
                let own = self.oci.own_ids();
                let dirs = vec![(lease.dir.clone(), Owner::Job)];
                ((own, own), dirs, JobNetwork::None)
            }
            false => {
                let ids = (self.oci.job_ids(), self.oci.container_ids());
                (ids, lease.to_make(), spec.network)
            }
        };
        for (dir, owner) in dirs {
            let (uid, gid) = match owner {
                Owner::Job => job,
                Owner::Container => job_id,
            };
            // A lease directory made beside the run's is the lifecycle's: it must be there,
            // empty and in the run's project, or it would be outside the run's bound.
            let made = match self.leases_beside && dir == lease.dir {
                true => take_dir(&dir, &spec.scratch, &project_of),
                false => make_dir(&dir),
            };
            made.and_then(|()| chown(&dir, uid, gid))
                .map_err(|why| Fault::step("make a lease directory", &why))?;
        }
        chown(&spec.scratch, job_id.0, job_id.1)
            .map_err(|why| Fault::step("give the run's scratch directory to the job", &why))?;
        if spec.files.is_some() {
            let files = self.files.as_ref().ok_or(Fault::NoFiles)?;
            files
                .hand_over_with(&spec.run_id, job_id.0, job_id.1, &chown)
                .map_err(|why| Fault::step("give the run's files to the job", &why))?;
        }
        let booking = Resources::new(spec.cpu_millis, spec.memory_bytes, 0, None)?;
        cgroup
            .create_with(&booking, None, LeasePolicy::Hard)
            .map_err(|why| Fault::step("make the lease's cgroup", &why))?;
        for file in JOB_OWNED_FILES {
            chown(&owned(cgroup.dir(), file), job.0, job.1)
                .map_err(|why| Fault::step("give the lease's cgroup to the job user", &why))?;
        }
        self.rt
            .block_on(net::create(&self.oci, container, network))
            .map_err(|why| Fault::step("write the bundle", &why))
    }

    /// Starts the container `id` that [`CrunRuntime::stage`] made and wires its network. A
    /// failure leaves it staged, or started with its job held by the init when the network
    /// could not be wired: the caller removes it, which ends it.
    ///
    /// # Errors
    /// The container is gone or started already, `crun` could not be started, or its
    /// network could not be wired.
    pub fn start_staged(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let _entered = self.rt.enter();
        let now = (self.hooks.now_ms)();
        let Some(run) = self.runs.get_mut(&id.0) else {
            return Err(RuntimeError::new("start: the container is gone"));
        };
        if !matches!(run.state, State::Created) {
            return Err(RuntimeError::new("start: the container was started"));
        }
        let started = self
            .oci
            .start_supervised(&run.spec, run.logs.file_cap(self.logs.as_ref()))
            .map_err(|why| RuntimeError::new(format!("start crun: {why}")))?;
        // The capture made both files before the supervisor said it was ready.
        let files = (stdout_path(&run.lease.dir), stderr_path(&run.lease.dir));
        run.logs
            .start(self.logs.as_ref(), (&files.0, &files.1), &run.lease.dir);
        run.backstop = run.cgroup.parent_ooms().ok();
        // The supervisor is on the sidecar before anything else is done, in the state still
        // starting, so a teardown at any point ends it. A failure to write is reported with
        // the running record's.
        let named = self
            .ledger
            .rewrite(&run.entry.supervised(Supervisor::of(started.id())));
        let (egress, now_ms) = (self.egress.as_deref(), self.hooks.now_ms);
        let wired = run
            .net
            .attach(egress, &self.oci, (&run.spec, &started), now_ms);
        // The sidecar says the run runs from here on: a later start adopts it. A failure to
        // write it leaves the run started, for the caller to remove.
        run.entry = run.entry.running(now, Supervisor::of(started.id()));
        let recorded = named.and(self.ledger.rewrite(&run.entry));
        run.state = State::Running {
            run: started,
            since_ms: now,
        };
        wired?;
        recorded.map_err(|why| RuntimeError::new(format!("record the running lease: {why}")))
    }

    /// Reads how the container `name` stands, ending it in the books once its supervisor wrote
    /// the end.
    fn status_of(&mut self, name: &str) -> Status {
        let (rt, oci) = (&self.rt, &self.oci);
        let Some(run) = self.runs.get_mut(name) else {
            return Status::Gone;
        };
        let (ended, captured) = match &run.state {
            State::Created => return Status::Running,
            State::Ended(exit, _) => return Status::Exited(*exit),
            State::Running { run: running, .. } => match running.try_end() {
                None => return Status::Running,
                // The drain of the streams is over once the supervisor wrote its record:
                // what it kept is all the run wrote.
                Some(end) => (rt.block_on(oci.ended_as(&run.spec, end)), running.tail()),
            },
        };
        let (exit, tail) = concluded(ended, &captured, (&run.cgroup, run.cap, run.backstop));
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

    /// Removes the container and everything its lease made. A handle this process does not
    /// hold is a leftover of an earlier process: it is removed if the ledger holds it as
    /// this agent's, nothing happens if the ledger does not know it, and the call fails
    /// (and changes nothing) if its sidecar is another agent's or is not well formed.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let (rt, oci, kill_wait) = (&self.rt, &self.oci, self.hooks.kill_wait);
        let Some(run) = self.runs.get_mut(&id.0) else {
            // Not held by this process: the leftover of an earlier one, if the ledger has
            // it, and nothing otherwise.
            return self.remove_leftover(&id.0).map(|_| ()).map_err(|why| {
                RuntimeError::new(format!("remove the leftovers of {}: {why}", id.0))
            });
        };
        let failed =
            |what: &str, why: &dyn std::fmt::Display| RuntimeError::new(format!("{what}: {why}"));
        if let State::Running { run: running, .. } = &mut run.state
            && running.try_end().is_none()
        {
            // Removed while it runs: ended by force, as a stop that spent its grace.
            let _ = run.cgroup.kill();
            let ended =
                rt.block_on(async { tokio::time::timeout(kill_wait, running.wait()).await });
            if ended.is_err() {
                let forced = rt.block_on(async {
                    tokio::time::timeout(kill_wait.max(FORCE_FLOOR), running.force()).await
                });
                forced.map_err(|_| {
                    failed(
                        "end the supervisor",
                        &format!("process {} is still there", running.id().pid),
                    )
                })?;
            }
        }
        run.net.release(&id.0);
        // No progress record is written from here on: a file made in the lease directory while
        // it is removed below would stop the removal.
        run.logs.leave();
        oci.remove(&id.0)
            .map_err(|why| failed("delete the container", &why))?;
        run.cgroup
            .remove()
            .map_err(|why| failed("remove the lease's cgroup", &why))?;
        if let Err(why) = js_contain::remove::remove_tree(&run.lease.dir)
            && why.kind() != io::ErrorKind::NotFound
        {
            return Err(failed("remove the lease directory", &why));
        }
        oci.unpin_image(&id.0)
            .map_err(|why| failed("remove the image's pin", &why))?;
        self.images.release(&id.0);
        self.ledger
            .forget(&id.0)
            .map_err(|why| failed("remove the lease's sidecar", &why))?;
        self.runs.remove(&id.0);
        Ok(())
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

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        if !self.leases_beside {
            return None;
        }
        let name = container_name(&self.host_id, run_id).ok()?;
        Some(self.oci.lease_dirs(&name).dir)
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

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        let run = self.runs.get(&id.0)?;
        let ctr = ctr_of(run.cgroup.dir());
        matches!(run.state, State::Running { .. })
            .then_some(ctr)
            .filter(|ctr| ctr.is_dir())
    }
}
