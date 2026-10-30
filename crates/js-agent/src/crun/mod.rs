//! The crun container runtime: [`CrunRuntime`], a [`Runtime`] over js-contain's `crun`
//! backend, for a root agent that starts every container as a dedicated non-root job user.
//!
//! # What a run becomes
//!
//! A run is one *lease*, named `js-<agent tag>-<run id>` (the name the other runtime gives
//! a container, so the two never name one alike). Its pieces:
//!
//! - the lease directory `<scratch>/<name>` and its `root`, `upper`, `work` and `rw/home`
//!   directories, made `0755` by the agent and given to the job user (the account `crun`
//!   keeps its state and log as) or to the job's own host id (what the job writes);
//! - the cgroup `<actions>/<name>`, made by the agent with the booking as the limit
//!   (memory the booking with no swap, CPU the booked cores: the arithmetic the lease
//!   server's booking assumes) and given to the job user, who makes the container's cgroup
//!   `<name>/ctr` under it ([`Runtime::cgroup`]);
//! - the run's scratch directory, given to the job's host id and bound at `/scratch`; the
//!   run's mounts, and its files at `/run/js-files` read-only, are bound after it;
//! - a bundle of the image's unpacked root (read-only), an overlay of an empty input root
//!   at `/work`, where the job starts, and the program and environment of the image's
//!   config with the run's arguments after the entrypoint.
//!
//! `crun run` is a foreground child of the agent, one thread each ([`Running`]), so the
//! runtime keeps its containers in memory: a container exists from its creation to its
//! [`Runtime::remove`], and [`Runtime::list_labelled`] lists those and no others. A
//! restarted agent finds none: `crun run` died with the process that started it.
//!
//! # Labels, the sweep and the lost run
//!
//! What a dead process leaves is files, and the runtime keeps a ledger of them: before a
//! lease's first directory is made, a sidecar file in `<scratch>/oci-labels` records the
//! agent's id, the run id, the lease's name and the container's labels (see
//! [`SweepReport`] and the module `ledger`), and it is removed after the last of the lease
//! is. [`CrunRuntime::open`] sweeps: it removes every lease whose sidecar names this
//! agent (the container's state, the cgroup, the directories, then the sidecar) and keeps
//! a [`SweepReport`] of what it did ([`CrunRuntime::swept`]). A sidecar of another agent,
//! or one that is not well formed, is left alone and reported, so two agents on one host
//! never remove each other's leases. A lease with no sidecar is not this runtime's.
//!
//! A sweep that cannot remove a lease keeps its sidecar, and the next start tries again.
//! A single leftover is removed by its run id with [`CrunRuntime::remove_orphan`] (and
//! [`Runtime::remove`] of a handle the runtime does not hold does the same).
//!
//! The run of a swept lease is lost, not adopted: the restarted agent lists no container,
//! so [`Lifecycle::adopt`](crate::lifecycle::Lifecycle::adopt) holds nothing, the agent's
//! first beat lists no run, and the lease server gives up the lease the dead process held
//! (at once when the new process names its predecessor stopped, after the handover grace
//! otherwise) and runs the work again from the farm's rerun budget.
//!
//! # Images
//!
//! [`Runtime::prepare`] pulls and unpacks in the background ([`Prefetcher`]); a run's image
//! must be pinned by a sha256 digest. [`Runtime::create_and_start`] asks the store again
//! (one `stat` for an image that is there, a pull otherwise: single flight with the
//! prefetcher's), and reads the image's config from the file the image source kept
//! ([`RegistryImages::keeping_configs`](crate::image::RegistryImages::keeping_configs),
//! [`config_dir`]).
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
//! exited with code 126; the words are in the lease's `crun.log` and init report until the
//! log tail (P12c) carries them.
//!
//! # Not here
//!
//! No network (a job has loopback only; `check_egress` has nothing to check), no GPUs
//! (`supports_gpus` is false and a start naming one is refused), no host jobs, no
//! read-back of the container's mounts, and no log tail.

mod ledger;
#[cfg(test)]
mod ledger_tests;
mod spec;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_contain::cgroup::{JOB_OWNED_FILES, LeaseCgroup, LeasePolicy, ROOT_OWNED_FILES};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::ImageSource;
use js_contain::oci::{LeaseDirs, Oci, OciConfig, OciError, Owner};
use js_contain::running::Running;
use js_types::Resources;
use tokio::runtime::{Builder, Runtime as TokioRuntime};

use crate::cli_runtime::{Chown, container_name, hand_over};
use crate::image::{ConfigError, ImageConfig};
use crate::lifecycle::{
    Body, Exit, FilesRoot, Found, Prefetcher, Prepared, PullFuture, RunHandle, RunSpec, Runtime,
    RuntimeError, Status, Ticket, label,
};
use ledger::{Entry, Ledger, Read};
pub use ledger::{Failed, Left, Removed, SweepReport};
use spec::{container_spec, exit_of, pinned};

/// How long a pull and unpack of one image may take before it counts as failed.
pub const PULL_LIMIT: Duration = Duration::from_secs(30 * 60);

/// How long a run is waited for after its cgroup was killed. A process in an
/// uninterruptible sleep outlasts it; the stop then fails and is tried again.
pub const KILL_WAIT: Duration = Duration::from_secs(60);

/// The directory of the image configs that the image source keeps and the runtime reads,
/// for a runtime whose store is `scratch`.
#[must_use]
pub fn config_dir(scratch: &Path) -> PathBuf {
    scratch.join("oci-configs")
}

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
}

/// What a runtime does that a test replaces: giving a file away and reading the clock.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Gives a path to a user and group.
    pub chown: Chown,
    /// Milliseconds since the Unix epoch.
    pub now_ms: fn() -> u64,
    /// How long a run is waited for after its cgroup was killed ([`KILL_WAIT`]).
    pub kill_wait: Duration,
    /// How long a pull and unpack of one image may take ([`PULL_LIMIT`]).
    pub pull_limit: Duration,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            chown: hand_over,
            now_ms: system_now_ms,
            kill_wait: KILL_WAIT,
            pull_limit: PULL_LIMIT,
        }
    }
}

fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Why a [`CrunRuntime`] could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum CrunError {
    /// The settings name no job user: the agent is root and a container runs as another.
    #[error("the crun runtime needs the job user the containers run as")]
    NoJobUser,
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
    #[error("the directory of the leases' labels {path}: {why}")]
    Ledger {
        /// The directory.
        path: PathBuf,
        /// The reason.
        why: io::Error,
    },
}

/// Why a run could not be made into a container.
#[derive(Debug, thiserror::Error)]
enum Fault {
    #[error("this runtime runs contained jobs only")]
    NotContained,
    #[error("containers on this runtime cannot use a GPU")]
    Gpu,
    #[error("{0}")]
    Image(String),
    #[error("run id {0:?} cannot name a container")]
    Name(String),
    #[error("a container of run {0} exists")]
    InUse(String),
    #[error("this runtime has no run files directory")]
    NoFiles,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Book(#[from] js_types::TypeError),
    #[error("{what}: {why}")]
    Step { what: &'static str, why: String },
}

impl Fault {
    fn step(what: &'static str, why: &dyn std::fmt::Display) -> Self {
        Self::Step {
            what,
            why: why.to_string(),
        }
    }
}

/// How far a container is.
enum State {
    /// Made and not started.
    Created,
    /// `crun run` runs (or has ended and not been read).
    Running { run: Running, since_ms: u64 },
    /// Ended, as read once.
    Ended(Exit),
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
}

/// A container runtime over `crun`.
pub struct CrunRuntime {
    oci: Arc<Oci>,
    pulls: Prefetcher,
    rt: TokioRuntime,
    cgroups: CgroupSettings,
    host_id: String,
    configs: PathBuf,
    files: Option<FilesRoot>,
    hooks: Hooks,
    runs: BTreeMap<String, Run>,
    ledger: Ledger,
    swept: SweepReport,
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
        settings.oci.job_user.ok_or(CrunError::NoJobUser)?;
        let oci = Oci::new(
            &settings.oci,
            source,
            &settings.scratch,
            &settings.cgroups.actions,
        )?;
        let rt = Builder::new_current_thread().enable_all().build()?;
        let (uid, gid) = oci.job_ids();
        let cgroups = settings.cgroups;
        // `crun`, run as the job user, moves its first process from the agent's cgroup into
        // the container's, which needs write access to the `cgroup.procs` of their nearest
        // common ancestor, and makes the container's cgroup in the lease's.
        let give = |dir: PathBuf, files: &[&str]| {
            files.iter().try_for_each(|file| {
                let path = dir.join(file);
                (hooks.chown)(&path, uid, gid).map_err(|why| CrunError::Delegation { path, why })
            })
        };
        give(cgroup_dir(&cgroups.mount, &cgroups.root), &ROOT_OWNED_FILES)?;
        give(
            cgroup_dir(&cgroups.mount, &cgroups.actions),
            &JOB_OWNED_FILES,
        )?;
        let configs = config_dir(&settings.scratch);
        std::fs::create_dir_all(&configs).map_err(|why| CrunError::Configs {
            path: configs.clone(),
            why,
        })?;
        let ledger = Ledger::open(&settings.scratch, &settings.host_id)
            .map_err(|(path, why)| CrunError::Ledger { path, why })?;
        let pulling = Arc::clone(&oci);
        let limit = hooks.pull_limit;
        let pulls = Prefetcher::new(settings.pull_threads, move |image| {
            pull(Arc::clone(&pulling), image, limit)
        })?;
        let mut runtime = Self {
            oci,
            pulls,
            rt,
            cgroups,
            host_id: settings.host_id,
            configs,
            files: settings.files,
            hooks,
            runs: BTreeMap::new(),
            ledger,
            swept: SweepReport::default(),
        };
        // No container of this process exists yet, and one of an earlier process of this
        // agent cannot run: its leases are leftovers.
        runtime.swept = runtime.sweep()?;
        Ok(runtime)
    }

    /// What the sweep at [`CrunRuntime::open`] did.
    #[must_use]
    pub fn swept(&self) -> &SweepReport {
        &self.swept
    }

    /// Removes what an earlier process of this agent left for the run `run_id` (the
    /// lease's container state, cgroup and directories, and its sidecar). Returns whether
    /// there was a lease to remove. A run this process holds is not an orphan: use
    /// [`Runtime::remove`].
    ///
    /// # Errors
    /// The run is held by this process, its sidecar is another agent's or is not well
    /// formed (it is left alone), or the removal failed (the sidecar stays, and the call
    /// can be made again).
    pub fn remove_orphan(&mut self, run_id: &str) -> Result<bool, RuntimeError> {
        let name = container_name(&self.host_id, run_id)
            .map_err(|_| RuntimeError::new(Fault::Name(run_id.to_owned()).to_string()))?;
        if self.runs.contains_key(&name) {
            return Err(RuntimeError::new(format!("run {run_id} is not an orphan")));
        }
        self.remove_leftover(&name)
            .map_err(|why| RuntimeError::new(format!("remove the leftovers of {name}: {why}")))
    }

    /// [`CrunRuntime::swept`]'s sweep: every lease of this agent in the ledger.
    fn sweep(&self) -> Result<SweepReport, CrunError> {
        self.ledger
            .sweep(|entry| self.tear_down(&entry.name))
            .map_err(|why| CrunError::Ledger {
                path: self.ledger.dir().to_owned(),
                why,
            })
    }

    /// Removes the lease `name` of an earlier process, if the ledger holds it as this
    /// agent's. Returns whether it did.
    fn remove_leftover(&self, name: &str) -> Result<bool, String> {
        match self.ledger.read(name) {
            Read::Absent => Ok(false),
            Read::Mine(entry) => {
                self.tear_down(&entry.name)?;
                self.ledger
                    .forget(&entry.name)
                    .map_err(|why| format!("remove the sidecar: {why}"))?;
                Ok(true)
            }
            Read::Foreign(entry) => Err(format!("it belongs to agent {:?}", entry.agent)),
            Read::Malformed(why) => Err(format!("its sidecar is not well formed: {why}")),
        }
    }

    /// Removes everything the lease `name` made: what still runs in its cgroup, the
    /// container's state, the cgroup and the directories. What is not there is skipped.
    fn tear_down(&self, name: &str) -> Result<(), String> {
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, name);
        let lease = self.oci.lease_dirs(name);
        if cgroup.dir().exists() {
            // A process the dead agent's keeper did not reach; the removals below retry.
            let _ = cgroup.kill();
        }
        if lease.dir.exists() {
            self.oci
                .remove(name)
                .map_err(|why| format!("delete the container: {why}"))?;
        }
        if cgroup.dir().exists() {
            cgroup
                .remove()
                .map_err(|why| format!("remove the lease's cgroup: {why}"))?;
        }
        match js_contain::remove::remove_tree(&lease.dir) {
            Err(why) if why.kind() != io::ErrorKind::NotFound => {
                Err(format!("remove the lease directory: {why}"))
            }
            _ => Ok(()),
        }
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
    /// As [`Runtime::create_and_start`].
    pub fn stage(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.make(spec)
            .map(RunHandle)
            .map_err(|fault| RuntimeError::new(fault.to_string()))
    }

    /// Makes the container of `spec`, everything but its start. A failure removes what it
    /// made.
    fn make(&mut self, spec: &RunSpec) -> Result<String, Fault> {
        if spec.isolation != js_agent_wire::Isolation::Contained {
            return Err(Fault::NotContained);
        }
        if !spec.gpu_indices.is_empty() {
            return Err(Fault::Gpu);
        }
        let Body::Container { image, args } = &spec.body;
        let image = pinned(image).map_err(Fault::Image)?;
        let name = container_name(&self.host_id, &spec.run_id)
            .map_err(|_| Fault::Name(spec.run_id.clone()))?;
        if self.runs.contains_key(&name) {
            return Err(Fault::InUse(spec.run_id.clone()));
        }
        self.rt
            .block_on(self.oci.check_pinned(&image))
            .map_err(|why| Fault::Image(why.to_string()))?;
        let config = ImageConfig::kept(&self.configs, &image.digest)?;
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, &name);
        let lease = self.oci.lease_dirs(&name);
        let mut container = container_spec(&name, spec, &image, &lease, cgroup.name());
        config.apply(args, &mut container)?;
        let now = (self.hooks.now_ms)();
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
        // Before the first thing the sidecar names is made (see the module `ledger`).
        let entry = Entry::new(&self.host_id, &spec.run_id, &name, &labels);
        self.ledger
            .record(&entry)
            .map_err(|why| Fault::step("record the lease's labels", &why))?;
        if let Err(fault) = self.make_resources(spec, &container, &lease, &cgroup) {
            // What the failed step left; none of it is a container. The sidecar goes with
            // it, or stays for the next sweep when some of it cannot be removed.
            let cgroup_gone = !cgroup.dir().exists() || cgroup.remove().is_ok();
            let dir_gone = match js_contain::remove::remove_tree(&lease.dir) {
                Ok(()) => true,
                Err(why) => why.kind() == io::ErrorKind::NotFound,
            };
            if cgroup_gone && dir_gone {
                let _ = self.ledger.forget(&name);
            }
            return Err(fault);
        }
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
            },
        );
        Ok(name)
    }

    /// The directories, hand-overs, cgroup and bundle of one lease.
    fn make_resources(
        &self,
        spec: &RunSpec,
        container: &ContainerSpec,
        lease: &LeaseDirs,
        cgroup: &LeaseCgroup,
    ) -> Result<(), Fault> {
        use std::os::unix::fs::PermissionsExt as _;
        let chown = self.hooks.chown;
        let (job, job_id) = (self.oci.job_ids(), self.oci.container_ids());
        for (dir, owner) in lease.to_make() {
            let (uid, gid) = match owner {
                Owner::Job => job,
                Owner::Container => job_id,
            };
            std::fs::create_dir(&dir)
                .and_then(|()| {
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
                })
                .and_then(|()| chown(&dir, uid, gid))
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
            chown(&cgroup.dir().join(file), job.0, job.1)
                .map_err(|why| Fault::step("give the lease's cgroup to the job user", &why))?;
        }
        self.rt
            .block_on(self.oci.create(container))
            .map_err(|why| Fault::step("write the bundle", &why))
    }

    /// Starts the container `id` that [`CrunRuntime::stage`] made. A failure leaves it
    /// staged: the caller removes it.
    ///
    /// # Errors
    /// The container is gone or started already, or `crun` could not be started.
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
            .start(&run.spec)
            .map_err(|why| RuntimeError::new(format!("start crun: {why}")))?;
        run.backstop = run.cgroup.parent_ooms().ok();
        run.state = State::Running {
            run: started,
            since_ms: now,
        };
        Ok(())
    }

    /// Reads how the container `name` stands, ending it in the books once `crun run` has.
    fn status_of(&mut self, name: &str) -> Status {
        let (rt, oci) = (&self.rt, &self.oci);
        let Some(run) = self.runs.get_mut(name) else {
            return Status::Gone;
        };
        let ended = match &run.state {
            State::Created => return Status::Running,
            State::Ended(exit) => return Status::Exited(*exit),
            State::Running { run: running, .. } if running.try_end().is_none() => {
                return Status::Running;
            }
            State::Running { run: running, .. } => rt.block_on(oci.ended(&run.spec, running)),
        };
        let oom = run.cgroup.oom_outcome(run.cap, run.backstop).ok().flatten();
        let exit = exit_of(ended, oom.as_ref());
        run.state = State::Ended(exit);
        Status::Exited(exit)
    }
}

/// The directory of the cgroup `name` (relative to `mount`, starting with `/`).
fn cgroup_dir(mount: &Path, name: &str) -> PathBuf {
    mount.join(name.trim_start_matches('/'))
}

/// The pull and unpack of `image`, with the limit of the whole.
fn pull(oci: Arc<Oci>, image: String, limit: Duration) -> PullFuture {
    Box::pin(async move {
        let pinned = pinned(&image)?;
        let pulled = tokio::time::timeout(limit, oci.check_pinned(&pinned)).await;
        pulled
            .map_err(|_| format!("pulling {image} took longer than {limit:?}"))?
            .map_err(|why| why.to_string())
    })
}

impl Runtime for CrunRuntime {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.pulls.prepare(image, waited)
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let id = self.stage(spec)?;
        if let Err(why) = self.start_staged(&id) {
            // The failed start leaves no container: the lease is taken apart as a removal
            // does.
            let _ = self.remove(&id);
            return Err(why);
        }
        Ok(id)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        Ok(self.status_of(&id.0))
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        let (rt, oci, kill_wait) = (&self.rt, &self.oci, self.hooks.kill_wait);
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
        let _ = rt.block_on(oci.terminate(&spec.name));
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
                rt.block_on(running.force());
            }
        }
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
                    State::Created | State::Ended(_) => 0,
                },
                never_started: matches!(run.state, State::Created),
            })
            .collect())
    }

    fn supports_gpus(&self) -> bool {
        false
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        let run = self.runs.get(&id.0)?;
        let ctr = run.cgroup.dir().join("ctr");
        matches!(run.state, State::Running { .. })
            .then_some(ctr)
            .filter(|ctr| ctr.is_dir())
    }
}
