//! Warm sandboxes: containers made and started ahead of their jobs, so that a job start only
//! binds its directories into one and hands its init the program (`docs/design/warm-sandboxes.md`).
//!
//! # The pool
//!
//! A runtime opened with [`CrunSettings::warm_sandboxes`](super::CrunSettings) above 0, with
//! a job file system ([`CrunSettings::fs`](super::CrunSettings), the new mount API) and its
//! leases beside the runs, keeps up to that many idle sandboxes for each image a contained job
//! started recently: none for an image no job started in the last [`KEEP_MS`], and otherwise
//! as many as started in the last [`RECENT_MS`], at least one. [`CrunRuntime::tend_warm`]
//! (the lifecycle's [`Runtime::tend`](crate::lifecycle::Runtime::tend), once a step, after the
//! step's starts) makes those an image lacks and removes those it no longer needs, and those
//! that ended. A sandbox is made as a run's container is (`make`, `held`), under a name of its
//! own (`w<epoch>-<n>`): its sidecar stays in the state starting until a run is started in it,
//! so a restarted agent removes every idle one as a lease that never ran; its lease is the
//! runtime's own, outside any run's quota, and holds only its bundle, its pipe and its logs.
//! Its cgroup books [`WARM_CPU_MILLIS`] and [`WARM_MEMORY_BYTES`] until a run takes it. Its
//! init, started by its supervisor's `crun run` as any container's, waits on the lease's pipe
//! ([`js_contain::oci::hand_job`]).
//!
//! # A start in a sandbox
//!
//! [`CrunRuntime::start_warm`] takes a sandbox of the run's image for a run that a held
//! container can carry (contained, no network, no GPU, no host port, no layered root, a
//! runtime that keeps none of these apart) and that is up (its container's group holds one
//! process, the init). Then, in order: the run's own lease directory, which the lifecycle made
//! beside its scratch directory under its quota, is taken and its directories made as a cold
//! lease's are; the run's scratch directory and files are given to the job; the lease's cgroup
//! is given the run's limits; the run's `/work` (the lease's upper directory, or a mount the run
//! names there), its home, its scratch directory, its mounts and its files are bound into the
//! container ([`js_contain::job_fs::bind_into`]), in the order `crun` binds them; the program,
//! its arguments, its directory and its environment, as the image's config and the run make
//! them, are handed to the init, which starts it; and the sidecar is written with the run's
//! labels and booking, in the state running, naming the sandbox's id as the one its name was
//! made from ([`Entry::lease_id`](super::ledger::Entry::lease_id)). A crash before that write
//! leaves an idle sandbox, which the next start removes, and the run is lost; after it, the
//! run is adopted as any other.
//!
//! A step that fails before the hand-over removes the sandbox and empties the run's lease
//! directory again, and the run is made cold, as if there had been no sandbox: nothing of the
//! run had started. The handle of a run started in a sandbox is the sandbox's name.
//!
//! What a sandbox never is: reused. A run's container is removed with the run, as a cold one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use js_agent_wire::Isolation;
use js_contain::LeasePolicy;
use js_contain::container::{ContainerSpec, HostMount};
use js_contain::job_fs::Inject;
use js_contain::oci::{Owner, Supervised, stderr_path, stdout_path};
use js_types::{JobNetwork, Resources};

use super::dirs::{make_dir, owned, project_of, take_dir};
use super::fs::Assembled;
use super::ledger::Supervisor;
use super::rootfs::RunFs;
use super::spec::{EXEC_ROOT, container_spec, ctr_of, job_env, pinned};
use super::{CrunRuntime, Run, State, net};
use crate::cli_runtime::{DEFAULT_PIDS, container_name};
use crate::image::ImageConfig;
use crate::lifecycle::{Body, RunHandle, RunSpec, RuntimeError, label};
use crate::logs::RunLogs;

/// How long after the last start of an image its sandboxes are kept: 10 minutes.
pub(super) const KEEP_MS: u64 = 10 * 60 * 1000;

/// The window whose starts of an image say how many sandboxes it gets: a minute.
pub(super) const RECENT_MS: u64 = 60 * 1000;

/// The CPU an idle sandbox's cgroup books: one core.
pub(super) const WARM_CPU_MILLIS: u64 = 1000;

/// The memory an idle sandbox's cgroup books: 64 MiB, for its init and `crun`'s setup.
pub(super) const WARM_MEMORY_BYTES: u64 = 64 << 20;

/// How long an image whose sandbox could not be made is not tried again: 30 seconds.
pub(super) const RETRY_MS: u64 = 30 * 1000;

/// Where the container sees the run's home directory (`js_contain`'s writable home).
const HOME: &str = "/root";

/// The idle sandboxes, the recent starts that size them, and the runs started in one.
pub(super) struct Pool {
    /// The most idle sandboxes of one image.
    per_image: u32,
    /// The calls that bind a run's directories into a sandbox.
    inject: Arc<dyn Inject + Send + Sync>,
    /// The idle sandboxes by lease name.
    idle: BTreeMap<String, Sandbox>,
    /// The starts of each image (as the run named it), in milliseconds, the last
    /// [`KEEP_MS`] of them.
    starts: BTreeMap<String, Vec<u64>>,
    /// The images whose last make failed, and until when they are not tried again.
    resting: BTreeMap<String, u64>,
    /// The part of every sandbox's id that names this runtime: when it opened.
    epoch: u64,
    /// The number of the next sandbox.
    next: u64,
    /// The handles of the runs started in a sandbox.
    warm_runs: BTreeSet<String>,
}

/// One idle sandbox.
pub(super) struct Sandbox {
    /// The image as the runs name it.
    image: String,
    /// Its id, which its name is made from.
    id: String,
    /// Its container, as made (its state is not read until a run is started in it).
    run: Run,
    /// The supervisor of its `crun run`; its init waits.
    running: Supervised,
}

impl Pool {
    /// A pool of at most `per_image` sandboxes for each image, binding with `inject`, whose
    /// ids carry `epoch`.
    pub(super) fn new(per_image: u32, inject: Arc<dyn Inject + Send + Sync>, epoch: u64) -> Self {
        Self {
            per_image,
            inject,
            idle: BTreeMap::new(),
            starts: BTreeMap::new(),
            resting: BTreeMap::new(),
            epoch,
            next: 0,
            warm_runs: BTreeSet::new(),
        }
    }

    /// A start of `image` at `now`; starts older than [`KEEP_MS`] are forgotten.
    pub(super) fn note_start(&mut self, image: &str, now: u64) {
        let starts = self.starts.entry(image.to_owned()).or_default();
        starts.push(now);
        starts.retain(|at| now.saturating_sub(*at) < KEEP_MS);
    }

    /// How many idle sandboxes `image` is to have at `now`: none when no start of it is
    /// younger than [`KEEP_MS`], otherwise the starts of the last [`RECENT_MS`], at least one
    /// and at most the pool's bound.
    pub(super) fn target(&self, image: &str, now: u64) -> usize {
        let starts = self.starts.get(image).map_or(&[][..], Vec::as_slice);
        let young = |within: u64| {
            starts
                .iter()
                .filter(|at| now.saturating_sub(**at) < within)
                .count()
        };
        match young(KEEP_MS) {
            0 => 0,
            _ => young(RECENT_MS).clamp(1, self.per_image as usize),
        }
    }

    /// How many idle sandboxes `image` has.
    fn count(&self, image: &str) -> usize {
        self.idle.values().filter(|s| s.image == image).count()
    }

    /// Takes out every idle sandbox to remove at `now`: those whose container ended, then
    /// each image's beyond its target, in the order of their names.
    fn take_unneeded(&mut self, now: u64) -> Vec<(String, Sandbox)> {
        let ended: Vec<String> = self
            .idle
            .iter()
            .filter(|(_, s)| s.running.try_end().is_some())
            .map(|(name, _)| name.clone())
            .collect();
        let mut gone: Vec<(String, Sandbox)> = ended
            .into_iter()
            .filter_map(|name| self.idle.remove(&name).map(|s| (name, s)))
            .collect();
        let images: BTreeSet<String> = self.idle.values().map(|s| s.image.clone()).collect();
        for image in images {
            let of_image = self.idle.iter().filter(|(_, s)| s.image == image);
            let names: Vec<String> = of_image.map(|(name, _)| name.clone()).collect();
            let surplus = names.len().saturating_sub(self.target(&image, now));
            let taken = names.into_iter().take(surplus);
            gone.extend(taken.filter_map(|name| self.idle.remove(&name).map(|s| (name, s))));
        }
        gone
    }

    /// What each image that started recently lacks at `now`, leaving out those that rest.
    fn lacking(&self, now: u64) -> Vec<(String, usize)> {
        let awake = |image: &&String| self.resting.get(*image).is_none_or(|until| now >= *until);
        let lack = |image: &String| self.target(image, now).saturating_sub(self.count(image));
        let images = self.starts.keys().filter(awake);
        images
            .map(|image| (image.clone(), lack(image)))
            .filter(|(_, lack)| *lack > 0)
            .collect()
    }

    /// `image` is not tried again until [`RETRY_MS`] after `now`.
    fn rest(&mut self, image: &str, now: u64) {
        self.resting
            .insert(image.to_owned(), now.saturating_add(RETRY_MS));
    }

    /// Takes out an idle sandbox of `image` that is up ([`init_of`]), with its init's pid.
    fn take_ready(&mut self, image: &str) -> Option<(String, u32, Sandbox)> {
        let of_image = self.idle.iter().filter(|(_, s)| s.image == image);
        let mut ready = of_image.filter_map(|(n, s)| init_of(s.run.cgroup.dir()).map(|p| (n, p)));
        let (name, pid) = ready.next().map(|(n, p)| (n.clone(), p))?;
        let sandbox = self.idle.remove(&name)?;
        Some((name, pid, sandbox))
    }

    /// The next sandbox's id.
    fn next_id(&mut self) -> String {
        self.next += 1;
        format!("w{}-{}", self.epoch, self.next)
    }

    /// The handles of the idle sandboxes.
    pub(super) fn idle_handles(&self) -> Vec<RunHandle> {
        self.idle.keys().cloned().map(RunHandle).collect()
    }

    /// Whether the run `handle` was started in a sandbox.
    pub(super) fn started_warm(&self, handle: &str) -> bool {
        self.warm_runs.contains(handle)
    }

    /// The supervisor of the idle sandbox `name`.
    pub(super) fn idle_supervisor(&mut self, name: &str) -> Option<&mut Supervised> {
        self.idle.get_mut(name).map(|sandbox| &mut sandbox.running)
    }

    /// The run `handle` is gone.
    pub(super) fn forget(&mut self, handle: &str) {
        self.warm_runs.remove(handle);
    }
}

/// Whether the run `spec` can be started in a held container, as far as the run says: a
/// contained run of an image, with no network, no GPU, no port and no host port. Gives its
/// image and arguments.
pub(super) fn fits(spec: &RunSpec) -> Option<(&str, &[String])> {
    let Body::Container { image, args } = &spec.body else {
        return None;
    };
    let plain = spec.isolation == Isolation::Contained
        && spec.network == JobNetwork::None
        && spec.gpu_indices.is_empty()
        && spec.ports.is_empty()
        && spec.host_ports.is_empty()
        && !image.is_empty();
    plain.then_some((image.as_str(), args.as_slice()))
}

/// The pid of the one process of the group `ctr` of a sandbox: its init, once the container
/// is up. None while `crun` sets it up (more than one, or none) or when it cannot be read.
pub(super) fn init_of(cgroup: &Path) -> Option<u32> {
    let procs = std::fs::read_to_string(ctr_of(cgroup).join("cgroup.procs")).ok()?;
    let mut pids = procs.split_whitespace();
    let pid = pids.next()?.parse().ok()?;
    pids.next().is_none().then_some(pid)
}

/// The binds that give the run of `container` its directories in a sandbox: its `/work` (the
/// run lease's upper directory, or the mount the run names there in its place), its home, then
/// the container's own mounts (its scratch directory, its mounts and its files) in order.
pub(super) fn binds_of(container: &ContainerSpec, (upper, home): (&Path, &Path)) -> Vec<HostMount> {
    let bind = |target: &str, source: &Path| HostMount {
        target: target.to_owned(),
        source: source.to_owned(),
        read_only: false,
        recursive: true,
        no_exec: false,
    };
    let at_exec = container.mounts.iter().position(|m| m.target == EXEC_ROOT);
    let work = at_exec.map_or_else(|| bind(EXEC_ROOT, upper), |at| container.mounts[at].clone());
    let others = container.mounts.iter().enumerate();
    let rest = others
        .filter(|(at, _)| Some(*at) != at_exec)
        .map(|(_, m)| m.clone());
    [work, bind(HOME, home)].into_iter().chain(rest).collect()
}

/// The text of a failure.
fn text(why: impl std::fmt::Display) -> String {
    why.to_string()
}

/// The words of the step `what` that failed, then why.
fn failed<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |why| format!("{what}: {why}")
}

impl CrunRuntime {
    /// The lifecycle's [`Runtime::tend`](crate::lifecycle::Runtime::tend): removes the idle
    /// sandboxes that ended and those an image no longer needs, then makes those an image
    /// lacks; an image whose make fails rests for [`RETRY_MS`].
    pub(super) fn tend_warm(&mut self, now: u64) {
        let Some(mut pool) = self.warm.take() else {
            return;
        };
        let gone = pool.take_unneeded(now);
        let lacking = pool.lacking(now);
        for (name, sandbox) in gone {
            self.discard_sandbox(name, sandbox.run, sandbox.running);
        }
        for (image, lack) in lacking {
            for _ in 0..lack {
                let made = self.make_warm(&mut pool, &image);
                if let Err(why) = made {
                    tracing::warn!(image = %image, "a container made ahead of its job: {why}");
                    pool.rest(&image, now);
                    break;
                }
            }
        }
        self.warm = Some(pool);
    }

    /// Makes and starts one sandbox of `image` and puts it in `pool`.
    fn make_warm(&mut self, pool: &mut Pool, image: &str) -> Result<(), String> {
        let id = pool.next_id();
        let name = container_name(&self.host_id, &id).map_err(text)?;
        let spec = RunSpec {
            run_id: id.clone(),
            body: Body::Container {
                image: image.to_owned(),
                args: Vec::new(),
            },
            env: Vec::new(),
            cpu_millis: WARM_CPU_MILLIS,
            memory_bytes: WARM_MEMORY_BYTES,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            // Its scratch directory is its own empty input root, until a run's is bound over
            // it: `/scratch` is there to bind over.
            scratch: self.oci.lease_dirs(&name).root,
            files: None,
            slot: 0,
            isolation: Isolation::Contained,
            network: JobNetwork::None,
            ports: Vec::new(),
            host_ports: Vec::new(),
            labels: BTreeMap::from([(label::WARM.to_owned(), image.to_owned())]),
            bundle: None,
            log_sink: None,
        };
        let made = self.make(&spec, None, true).map_err(text)?;
        let mut run = self.runs.remove(&made).ok_or("the sandbox is gone")?;
        // Started as a run's container is, its capture capped as a run with live logs would
        // be (a sandbox does not know whether its run will have them). Its sidecar stays in
        // the state starting.
        let cap = self.logs.as_ref().map(|logs| logs.file_cap);
        let running = match self.oci.start_supervised(&run.spec, cap) {
            Ok(running) => running,
            Err(why) => {
                self.discard_unstarted(made, run);
                return Err(format!("start crun: {why}"));
            }
        };
        run.entry = run.entry.supervised(Supervisor::of(running.id()));
        // Not a failure when it cannot be written: the lease directory names the supervisor
        // too, and a restart's teardown ends it from there.
        let _ = self.ledger.rewrite(&run.entry);
        let sandbox = Sandbox {
            image: image.to_owned(),
            id,
            run,
            running,
        };
        pool.idle.insert(made, sandbox);
        Ok(())
    }

    /// Removes the container `name` (supervised by `running`) and everything its lease made.
    fn discard_sandbox(&mut self, name: String, mut run: Run, running: Supervised) {
        run.state = State::Running {
            run: running,
            since_ms: 0,
        };
        self.discard_unstarted(name, run);
    }

    /// Removes the container `run` named `name` and everything its lease made, as a removal
    /// does. One that cannot be removed is left to the next start of the agent, whose sweep
    /// removes it: it is never a run of this process.
    fn discard_unstarted(&mut self, name: String, run: Run) {
        let handle = RunHandle(name.clone());
        self.runs.insert(name, run);
        if let Err(why) = crate::lifecycle::Runtime::remove(self, &handle) {
            tracing::warn!(lease = %handle.0, "remove a container made ahead of its job: {why}");
            self.runs.remove(&handle.0);
        }
    }

    /// Starts the run `spec` in an idle sandbox of its image, when it can be ([`fits`], no
    /// layered root, a sandbox that is up): `Err` with `assembled` given back when it cannot,
    /// and the run is then made cold; a failure before the hand-over removes the sandbox and is
    /// `Err` too. Every start that fits is counted towards its image's sandboxes. `assembled`
    /// is what the run's assembly made.
    pub(super) fn start_warm(
        &mut self,
        spec: &RunSpec,
        assembled: Option<Assembled>,
        now: u64,
    ) -> Result<Result<RunHandle, RuntimeError>, Option<Assembled>> {
        let Some(mut pool) = self.warm.take() else {
            return Err(assembled);
        };
        let started = self.start_in(&mut pool, spec, assembled, now);
        self.warm = Some(pool);
        started
    }

    /// [`CrunRuntime::start_warm`] with the pool taken out of the runtime.
    fn start_in(
        &mut self,
        pool: &mut Pool,
        spec: &RunSpec,
        assembled: Option<Assembled>,
        now: u64,
    ) -> Result<Result<RunHandle, RuntimeError>, Option<Assembled>> {
        let unlayered = assembled.as_ref().is_none_or(|made| made.root.is_none());
        let Some((image, args)) = fits(spec).filter(|_| unlayered) else {
            return Err(assembled);
        };
        pool.note_start(image, now);
        let Some((name, pid, sandbox)) = pool.take_ready(image) else {
            return Err(assembled);
        };
        let Sandbox {
            id,
            mut run,
            running,
            ..
        } = sandbox;
        let inject = Arc::clone(&pool.inject);
        let bound = self.bind(
            spec,
            (image, args),
            (&name, pid),
            &mut run,
            (assembled.as_ref(), &*inject),
        );
        let handed = bound.and_then(|c| js_contain::oci::hand_job(&c).map(|()| c).map_err(text));
        match handed {
            Ok(container) => {
                pool.warm_runs.insert(name.clone());
                let ids = (name.as_str(), id.as_str(), container.image_digest.as_str());
                Ok(self.record_warm(spec, ids, (run, running), assembled, now))
            }
            Err(why) => {
                tracing::warn!(lease = %name, "start a run in a container made ahead of it: {why}");
                self.unbind_run_lease(spec);
                self.discard_sandbox(name, run, running);
                Err(assembled)
            }
        }
    }

    /// Prepares the sandbox `run` (named `name`, its init `pid`) for the run `spec` of
    /// `image` with `args` up to the hand-over, and gives the container spec whose job is to
    /// be handed (in the sandbox's lease directory). See the module's documentation for the
    /// steps.
    fn bind(
        &mut self,
        spec: &RunSpec,
        (given, args): (&str, &[String]),
        (name, pid): (&str, u32),
        run: &mut Run,
        (assembled, inject): (Option<&Assembled>, &(dyn Inject + Send + Sync)),
    ) -> Result<ContainerSpec, String> {
        let image = pinned(given)?;
        let run_name = container_name(&self.host_id, &spec.run_id).map_err(text)?;
        let run_lease = self.oci.lease_dirs(&run_name);
        let mut container = container_spec(name, spec, &image, &run.lease, run.cgroup.name());
        assembled
            .into_iter()
            .for_each(|made| Self::give(&mut container, made));
        let config = ImageConfig::kept(&self.configs, &image.digest).map_err(text)?;
        config.apply(args, &mut container).map_err(text)?;
        job_env(&mut container, &spec.env);
        // The run's lease directory, which the lifecycle made under the run's quota, takes
        // the run's writes to `/work` and its home, as a cold lease's would.
        let chown = self.hooks.chown;
        let ids = (self.oci.job_ids(), self.oci.container_ids());
        for (dir, owner) in run_lease.to_make() {
            let (uid, gid) = match owner {
                Owner::Job => ids.0,
                Owner::Container => ids.1,
            };
            let made = match dir == run_lease.dir {
                true => take_dir(&dir, &spec.scratch, &project_of),
                false => make_dir(&dir),
            };
            let given = made.and_then(|()| chown(&dir, uid, gid));
            given.map_err(failed("make the run's lease directory"))?;
        }
        let scratch = chown(&spec.scratch, ids.1.0, ids.1.1);
        scratch.map_err(failed("give the run's scratch directory to the job"))?;
        if spec.files.is_some() {
            let files = self
                .files
                .as_ref()
                .ok_or("this runtime has no run files directory")?;
            let given = files.hand_over_with(&spec.run_id, ids.1.0, ids.1.1, &chown);
            given.map_err(failed("give the run's files to the job"))?;
        }
        let booking = Resources::new(spec.cpu_millis, spec.memory_bytes, 0, None).map_err(text)?;
        let limits = run.cgroup.set_limits(&booking, LeasePolicy::Hard);
        limits.map_err(failed("set the lease's limits"))?;
        let pids = spec.pids.unwrap_or(DEFAULT_PIDS).to_string();
        let written = std::fs::write(owned(&ctr_of(run.cgroup.dir()), "pids.max"), pids);
        written.map_err(failed("set the container's process limit"))?;
        run.cap = js_contain::hard_memory_max(spec.memory_bytes);
        run.backstop = run.cgroup.parent_ooms().ok();
        let binds = binds_of(&container, (&run_lease.upper, &run_lease.home));
        let bound = js_contain::job_fs::bind_into(inject, pid, &binds, ids.1);
        bound.map_err(failed("bind the run's directories"))?;
        container.dir.clone_from(&run.lease.dir);
        Ok(container)
    }

    /// Empties the run's lease directory of what [`CrunRuntime::bind`] made in it, so that the
    /// cold make takes it again. What cannot be removed stays, and the cold make refuses the
    /// directory then.
    fn unbind_run_lease(&self, spec: &RunSpec) {
        let run_name = container_name(&self.host_id, &spec.run_id).unwrap_or_default();
        let made = self.oci.lease_dirs(&run_name).to_make();
        for (dir, _) in made.iter().skip(1).rev() {
            let _ = js_contain::remove::remove_tree(dir);
        }
    }

    /// Books the run `spec` started in the sandbox `name` (of the id `id`, the image
    /// `digest`), whose container `run` runs under `running`: its labels, its live logs, its
    /// sources, and its sidecar in the state running. A sidecar that cannot be written fails
    /// the start, and the caller removes the run.
    fn record_warm(
        &mut self,
        spec: &RunSpec,
        (name, id, digest): (&str, &str, &str),
        (mut run, running): (Run, Supervised),
        assembled: Option<Assembled>,
        now: u64,
    ) -> Result<RunHandle, RuntimeError> {
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
        let mut entry = self.entry_for(spec, name, (&labels, digest));
        entry.lease_id = Some(id.to_owned());
        run.entry = entry.running(now, Supervisor::of(running.id()));
        run.state = State::Running {
            run: running,
            since_ms: now,
        };
        run.labels = labels;
        run.net = net::Net::asked(JobNetwork::None, Vec::new(), Vec::new());
        let run_name = container_name(&self.host_id, &spec.run_id).unwrap_or_default();
        run.fs = assembled.map(|made| RunFs::assembled_as(made.sources, run_name));
        run.logs = RunLogs::asked(self.logs.as_ref(), spec.log_sink.as_ref());
        let files = (stdout_path(&run.lease.dir), stderr_path(&run.lease.dir));
        let lease = run.lease.dir.clone();
        run.logs
            .start(self.logs.as_ref(), (&files.0, &files.1), &lease);
        let recorded = self.ledger.rewrite(&run.entry);
        self.runs.insert(name.to_owned(), run);
        recorded.map_err(|why| RuntimeError::new(format!("record the running lease: {why}")))?;
        Ok(RunHandle(name.to_owned()))
    }
}
