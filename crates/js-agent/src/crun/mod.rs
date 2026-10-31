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
//!   at `/work`, where the job starts, and the program and environment of the image's
//!   config with the run's arguments after the entrypoint.
//!
//! `crun run` is a foreground child of the agent, one thread each ([`Running`]), so the
//! runtime keeps its containers in memory: a container exists from its creation to its
//! [`Runtime::remove`]. A restarted agent finds none (labels in a file, a sweep and the
//! lost-run path are P12b's).
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

mod claim;
mod spec;
#[cfg(test)]
mod tests;
mod wire;

pub use claim::{ClaimError, HostClaim};

pub use wire::{
    ACTIONS_DIR, AGENT_LEAF, CgroupError, CgroupSource, ID_TOOL_PATH, IdSources, PULL_THREADS,
    Seams, WireError, oci_config, open as open_for_plan,
};

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_contain::cgroup::{JOB_OWNED_FILES, LeaseCgroup, LeasePolicy, ROOT_OWNED_FILES};
use js_contain::container::ContainerSpec;
use js_contain::oci::image::{ImageSource, PinnedImage};
use js_contain::oci::{LeaseDirs, Oci, OciConfig, OciError, Owner};
use js_contain::running::Running;
use js_types::Resources;
use tokio::runtime::{Builder, Runtime as TokioRuntime};

use crate::cli_runtime::{Chown, container_name};
use crate::image::{ConfigError, ImageConfig, config_path};
use crate::lifecycle::{
    Body, Exit, FilesRoot, Found, Prefetcher, Prepared, PullFuture, RunHandle, RunSpec, Runtime,
    RuntimeError, Status, Ticket, label,
};
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
#[derive(Clone, Copy, Debug)]
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
            chown: hand_over_no_follow,
            now_ms: system_now_ms,
            kill_wait: KILL_WAIT,
            pull_limit: PULL_LIMIT,
        }
    }
}

/// Gives the file or directory `path` to `uid` and `gid` by the descriptor of what it opens,
/// without following a link: a path replaced by a link is refused, and what is given is what
/// was opened.
///
/// # Errors
/// The system's reason: the path is not there, is a link, or may not be given away.
pub fn hand_over_no_follow(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, fchown};
    use rustix::process::{Gid, Uid};
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    rustix::fs::open(path, flags, Mode::empty())
        .and_then(|fd| fchown(&fd, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))))
        .map_err(io::Error::from)
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
    /// The cgroup of the helper containers could not be made or given to the job user.
    #[error("the cgroup of the helper containers {path}: {why}")]
    Helpers {
        /// The cgroup.
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
    #[error("remove an image that has no config: {0}")]
    Evict(#[from] io::Error),
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
        let oci = Oci::new(&settings.oci, source, &settings.scratch, &helpers)?;
        let rt = Builder::new_current_thread().enable_all().build()?;
        let configs = config_dir(&settings.scratch);
        std::fs::create_dir_all(&configs).map_err(|why| CrunError::Configs {
            path: configs.clone(),
            why,
        })?;
        let pulling = Arc::clone(&oci);
        let limit = hooks.pull_limit;
        let pulls = Prefetcher::new(settings.pull_threads, move |image| {
            pull(Arc::clone(&pulling), image, limit)
        })?;
        Ok(Self {
            oci,
            pulls,
            rt,
            cgroups,
            host_id: settings.host_id,
            configs,
            files: settings.files,
            hooks,
            runs: BTreeMap::new(),
        })
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
    ///
    /// An image in the store with no kept config is removed and fetched again. That deletes
    /// the root such an image gives as the lower layer of a container's mounts, so it relies
    /// on no container of an earlier daemon still running on it: the start-up sweep that ends
    /// what an earlier agent left (P12b) must have run before the first run is made. The
    /// removal and a fetch of the same image never overlap (the image store holds one gate
    /// for each digest).
    fn make(&mut self, spec: &RunSpec) -> Result<String, Fault> {
        if spec.isolation != js_agent_wire::Isolation::Contained {
            return Err(Fault::NotContained);
        }
        if !spec.gpu_indices.is_empty() {
            return Err(Fault::Gpu);
        }
        let Body::Container { image: given, args } = &spec.body;
        let image = pinned(given).map_err(Fault::Image)?;
        let name = container_name(&self.host_id, &spec.run_id)
            .map_err(|_| Fault::Name(spec.run_id.clone()))?;
        if self.runs.contains_key(&name) {
            return Err(Fault::InUse(spec.run_id.clone()));
        }
        let limit = self.hooks.pull_limit;
        self.rt
            .block_on(ensure_image(&self.oci, &image, limit))
            .map_err(Fault::Image)?;
        if !config_path(&self.configs, &image.digest).exists() {
            // An image an earlier daemon (or a source that keeps no configs) unpacked has
            // no config to run it by: it is fetched again, and the source keeps the config
            // this time. The prefetcher is told, as it is for any image taken out.
            self.rt.block_on(self.oci.remove_image(&image))?;
            self.pulls.forget(given);
            self.rt
                .block_on(ensure_image(&self.oci, &image, limit))
                .map_err(Fault::Image)?;
        }
        let config = ImageConfig::kept(&self.configs, &image.digest)?;
        let cgroup = LeaseCgroup::at(&self.cgroups.mount, &self.cgroups.actions, &name);
        let lease = self.oci.lease_dirs(&name);
        let mut container = container_spec(&name, spec, &image, &lease, cgroup.name());
        config.apply(args, &mut container)?;
        if let Err(fault) = self.make_resources(spec, &container, &lease, &cgroup) {
            // What the failed step left; none of it is a container.
            let _ = cgroup.remove();
            let _ = js_contain::remove::remove_tree(&lease.dir);
            return Err(fault);
        }
        let now = (self.hooks.now_ms)();
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
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
        let chown = self.hooks.chown;
        let (job, job_id) = (self.oci.job_ids(), self.oci.container_ids());
        for (dir, owner) in lease.to_make() {
            let (uid, gid) = match owner {
                Owner::Job => job,
                Owner::Container => job_id,
            };
            make_dir(&dir)
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
            chown(&owned(cgroup.dir(), file), job.0, job.1)
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

/// Makes the new directory `dir` with the mode `0755` whatever the umask is, setting the mode
/// by the descriptor of what was made.
fn make_dir(dir: &Path) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, fchmod};
    std::fs::create_dir(dir)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::open(dir, flags, Mode::empty())
        .and_then(|fd| fchmod(&fd, Mode::from_raw_mode(0o755)))
        .map_err(io::Error::from)
}

/// The path of `file` of the cgroup `dir` in [`JOB_OWNED_FILES`], where the empty name is the
/// directory itself (not `dir/`, whose trailing slash would make a no-follow open follow).
fn owned(dir: &Path, file: &str) -> PathBuf {
    if file.is_empty() {
        dir.to_owned()
    } else {
        dir.join(file)
    }
}

/// Makes the cgroup `dir` of the helper containers (once: one that is there is kept), hands
/// the controllers to its children and gives it to the job user.
fn make_helper_cgroup(dir: &Path, chown: Chown, (uid, gid): (u32, u32)) -> io::Result<()> {
    if !dir.is_dir() {
        std::fs::create_dir(dir)?;
    }
    std::fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    JOB_OWNED_FILES
        .iter()
        .try_for_each(|file| chown(&dir.join(file), uid, gid))
}

/// The directory of the cgroup `name` (relative to `mount`, starting with `/`).
fn cgroup_dir(mount: &Path, name: &str) -> PathBuf {
    mount.join(name.trim_start_matches('/'))
}

/// `image` in the store, fetched and unpacked if it is not, within `limit` for the whole.
async fn ensure_image(oci: &Oci, image: &PinnedImage, limit: Duration) -> Result<(), String> {
    let ensured = tokio::time::timeout(limit, oci.check_pinned(image)).await;
    ensured
        .map_err(|_| format!("pulling {} took longer than {limit:?}", image.reference))?
        .map_err(|why| why.to_string())
}

/// The pull and unpack of `image`, with the limit of the whole.
fn pull(oci: Arc<Oci>, image: String, limit: Duration) -> PullFuture {
    Box::pin(async move {
        let pinned = pinned(&image)?;
        ensure_image(&oci, &pinned, limit).await
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
            return Ok(());
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
