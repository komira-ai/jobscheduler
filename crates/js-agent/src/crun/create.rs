//! The make and start of a lease, on the worker or off it.
//!
//! A start writes and syncs the lease's sidecar, pins and makes sure of its image, makes its
//! directories and cgroup, writes its bundle, spawns its supervisor and waits for its ready
//! word, wires its network and syncs the sidecar again: work that waits on the disk and on
//! other processes. [`Runtime::begin_create`](crate::lifecycle::Runtime::begin_create) runs it
//! on a thread of the create pool ([`THREADS`] threads, `js-create`), so the lifecycle's
//! worker goes on with every other job while one disk sync is slow, and the start that waits
//! on it is the only one held.
//!
//! The worker keeps what reads or changes the runtime's own books: the checks of the start
//! (its host isolation, its network, its GPUs), the taking of its assembled file system, its
//! name (refused while a container or a create of it exists) and its sidecar's contents
//! ([`Head`]). The thread is given a [`Maker`]: copies of the settings and shares of the
//! image store, the ledger, the egress and the forwarder, nothing of the worker's. It answers
//! the made [`Run`], which the worker puts in its books when it next asks
//! ([`CrunRuntime::poll_create`](crate::lifecycle::Runtime::poll_create)), or the failure. A
//! failure is taken apart on the worker, as a failed start always was: a lease whose make
//! failed through the ledger's discard, a container whose start failed by its removal.
//!
//! The sidecar is recorded in the state starting before anything it names is made, and
//! rewritten once, in the state running with the supervisor, after the network is wired. A
//! teardown in between finds the supervisor by the record it keeps in the lease directory
//! (the ledger's teardown reads it when the sidecar names none). A lease whose create is under
//! way is not removed: its name is refused to a removal until the create answered.
//!
//! A create has no deadline of its own: its waits are bounded where they are made (the
//! supervisor's ready word, the network's route), and a call that hangs in the kernel (a disk
//! or a file system that never answers) holds its thread and its start, which stays
//! `starting` with its slot booked; a stop of it waits for the answer. Such a start holds one
//! of the [`THREADS`] and no other job, where it held the whole worker before.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use js_contain::cgroup::LeaseCgroup;
use js_contain::container::ContainerSpec;
use js_contain::oci::image::PinnedImage;
use js_contain::oci::{LeaseDirs, Oci, stderr_path, stdout_path};
use tokio::runtime::Runtime as TokioRuntime;

use super::fault::Fault;
use super::fs::Assembled;
use super::fs::pool::{Pending, Poll, Pool};
use super::ledger::{Entry, Ledger, Supervisor};
use super::make::Marks;
use super::pull::Images;
use super::{CgroupSettings, CrunRuntime, EgressPort, Hooks, Run, State, gpu, net, rootfs};
use crate::cli_runtime::container_name;
use crate::host_ports::Forwarder;
use crate::lifecycle::{
    Body, Created, FilesRoot, Forgetter, RunHandle, RunSpec, RuntimeError, check_run_env, label,
};
use crate::logs::{LiveLogs, RunLogs};

/// The threads of the create pool: as many starts are made at once, the rest wait their
/// turn in the order they were begun.
pub(super) const THREADS: usize = 4;

/// What a thread of the create pool answers: the made run and how its start went, or the
/// failure of its make and the sidecar that was recorded for it, if one was.
type Answer = Result<(Run, Result<(), RuntimeError>), Unmade>;

/// Why a lease was not made, and the sidecar recorded for it, if one was: the worker takes
/// apart what the make left by it.
type Unmade = (Fault, Option<Box<Entry>>);

/// The creates under way: by lease name, the answer to come and the sidecar the worker
/// takes the lease apart by when the thread never answers.
pub(super) struct Creates {
    pool: Pool,
    under_way: BTreeMap<String, (Pending<Answer>, Entry)>,
}

impl Creates {
    /// The pool of [`THREADS`] threads, with nothing under way.
    ///
    /// # Errors
    /// A thread could not be made.
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            pool: Pool::named(THREADS, "js-create")?,
            under_way: BTreeMap::new(),
        })
    }

    /// Rings `wake` whenever a create begun from now on has answered.
    pub(super) fn set_wake(&mut self, wake: crate::lifecycle::Wake) {
        self.pool.set_wake(wake);
    }

    /// Whether a create of the lease `name` is under way.
    pub(super) fn holds(&self, name: &str) -> bool {
        self.under_way.contains_key(name)
    }
}

/// What the making of a lease needs, shared with the threads of the create pool: copies of
/// the runtime's settings and shares of what is safe to use from many threads at once (the
/// image store and its budget, the ledger's directory, the egress, the forwarder).
#[derive(Clone)]
pub(super) struct Maker {
    pub(super) oci: Arc<Oci>,
    pub(super) images: Arc<Images>,
    pub(super) forget: Forgetter,
    pub(super) rt: Arc<TokioRuntime>,
    pub(super) cgroups: CgroupSettings,
    pub(super) configs: PathBuf,
    pub(super) files: Option<FilesRoot>,
    pub(super) leases_beside: bool,
    pub(super) store: PathBuf,
    pub(super) egress: Option<Arc<dyn EgressPort>>,
    pub(super) forwarder: Arc<Forwarder>,
    pub(super) hooks: Hooks,
    pub(super) ledger: Ledger,
    pub(super) logs: Option<LiveLogs>,
}

/// What the worker settles of a start before its lease is made.
pub(super) struct Head {
    name: String,
    host: bool,
    given: String,
    args: Vec<String>,
    image: PinnedImage,
    gpu: gpu::Wiring,
    assembled: Option<Assembled>,
    labels: BTreeMap<String, String>,
    entry: Entry,
    marks: Marks,
}

impl CrunRuntime {
    /// What a thread of the create pool makes a lease with.
    pub(super) fn maker(&self) -> Maker {
        Maker {
            oci: Arc::clone(&self.oci),
            images: Arc::clone(&self.images),
            forget: self.pulls.forgetter(),
            rt: Arc::clone(&self.rt),
            cgroups: self.cgroups.clone(),
            configs: self.configs.clone(),
            files: self.files.clone(),
            leases_beside: self.leases_beside,
            store: self.store.clone(),
            egress: self.egress.clone(),
            forwarder: Arc::clone(&self.host_ports.1),
            hooks: self.hooks,
            ledger: self.ledger.clone(),
            logs: self.logs.clone(),
        }
    }

    /// Makes the container of `spec` and does not start it: the first half of
    /// [`Runtime::create_and_start`](crate::lifecycle::Runtime::create_and_start), for a
    /// caller that starts it later (or never, as the conformance suite does). Such a
    /// container lists as [`never_started`](crate::lifecycle::Found::never_started).
    ///
    /// # Errors
    /// As [`Runtime::create_and_start`](crate::lifecycle::Runtime::create_and_start), and an
    /// environment [`check_run_env`] refuses.
    pub fn stage(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        check_run_env(&spec.env)?;
        let head = self.head(spec).map_err(failed)?;
        let made = self.maker().make(spec, head);
        self.made(made.map(|run| (run, Ok(()))))
    }

    /// Starts the container `id` that [`CrunRuntime::stage`] made and wires its network. A
    /// failure leaves it staged, or started with its job held by the init when the network
    /// could not be wired: the caller removes it, which ends it.
    ///
    /// # Errors
    /// The container is gone or started already, `crun` could not be started, or its
    /// network could not be wired.
    pub fn start_staged(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let maker = self.maker();
        let Some(run) = self.runs.get_mut(&id.0) else {
            return Err(RuntimeError::new("start: the container is gone"));
        };
        if !matches!(run.state, State::Created) {
            return Err(RuntimeError::new("start: the container was started"));
        }
        maker.start(run)
    }

    /// Begins the make and start of `spec` on a thread of the create pool: pending, or the
    /// failure of a check made on the worker first.
    pub(super) fn begin_off_worker(&mut self, spec: &RunSpec) -> Created {
        let head = check_run_env(&spec.env).and_then(|()| self.head(spec).map_err(failed));
        let head = match head {
            Ok(head) => head,
            Err(why) => return Created::Done(Err(why)),
        };
        let (name, entry) = (head.name.clone(), head.entry.clone());
        let (maker, spec) = (self.maker(), spec.clone());
        let pending = self.creates.pool.run(u64::MAX, move || {
            let mut run = maker.make(&spec, head)?;
            let started = maker.start(&mut run);
            Ok((run, started))
        });
        self.creates.under_way.insert(name, (pending, entry));
        Created::Pending
    }

    /// How the create of the run `run_id` stands: once it answered, the made run is in the
    /// books (and removed again when its start failed) and the create is forgotten.
    pub(super) fn poll_off_worker(&mut self, run_id: &str) -> Created {
        let name = container_name(&self.host_id, run_id).unwrap_or_default();
        let Some((pending, entry)) = self.creates.under_way.remove(&name) else {
            let why = format!("no create of {run_id} is under way");
            return Created::Done(Err(RuntimeError::new(why)));
        };
        let answer = match pending.poll(0) {
            Poll::Waiting => {
                self.creates.under_way.insert(name, (pending, entry));
                return Created::Pending;
            }
            Poll::Answered(answer) => answer,
            // A thread that never answered (it panicked) may have recorded the sidecar and
            // made some of the lease: it is taken apart by the sidecar the worker kept.
            Poll::Unanswered(why) => {
                Err((Fault::step("make the lease", &why), Some(Box::new(entry))))
            }
        };
        Created::Done(self.made(answer))
    }

    /// What the worker settles of `spec` before its lease is made (see [`Head`]): the checks
    /// that read its books, its name and its sidecar.
    fn head(&mut self, spec: &RunSpec) -> Result<Head, Fault> {
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
        let name = container_name(&self.host_id, &spec.run_id)
            .map_err(|_| Fault::Name(spec.run_id.clone()))?;
        if self.runs.contains_key(&name) || self.creates.holds(&name) {
            return Err(Fault::InUse(spec.run_id.clone()));
        }
        // What the run's assembly made of its layers and mounts: the run's removal unmounts
        // it, or, for a run that is not made, the lifecycle's discard (`Runtime::discard_fs`).
        let assembled = self.fs.as_mut().and_then(|fs| fs.take(&spec.run_id));
        // A root of layers alone has no image.
        let image = match given.is_empty() {
            true => PinnedImage {
                reference: String::new(),
                digest: String::new(),
            },
            false => super::spec::pinned(given).map_err(Fault::Image)?,
        };
        let now = (self.hooks.now_ms)();
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), self.host_id.clone());
        labels.insert(label::CREATED_MS.to_owned(), now.to_string());
        // A restart offers the run its host ports again from this.
        if let Some(host_ports) = net::host_ports_label(&spec.host_ports) {
            labels.insert(label::HOST_PORTS.to_owned(), host_ports);
        }
        let entry = self.entry_for(spec, &name, (&labels, &image.digest));
        Ok(Head {
            name,
            host,
            given: given.clone(),
            args: args.clone(),
            image,
            gpu,
            assembled,
            labels,
            entry,
            marks: vec![("begin", now)],
        })
    }

    /// Puts what a make answered in the books: the run, removed again when its start failed;
    /// or, for a make that failed, what it left taken apart through the ledger's discard
    /// (the sidecar goes with it, the pin last, or stays for the next start when some of it
    /// cannot be removed).
    fn made(&mut self, answer: Answer) -> Result<RunHandle, RuntimeError> {
        let (run, started) = match answer {
            Ok(made) => made,
            Err((fault, recorded)) => {
                if let Some(entry) = recorded {
                    let _ = self.ledger.discard(&entry, |entry| self.tear_down(entry));
                }
                return Err(failed(fault));
            }
        };
        let id = RunHandle(run.entry.name.clone());
        self.runs.insert(id.0.clone(), run);
        if let Err(why) = started {
            // The failed start leaves no container: the lease is taken apart as a removal
            // does. What cannot be is kept, listed, and removed by the next removal or sweep.
            if let Err(left) = crate::lifecycle::Runtime::remove(self, &id) {
                tracing::warn!(lease = %id.0, "remove the failed start: {left}");
            }
            return Err(why);
        }
        Ok(id)
    }
}

/// The runtime's failure for `fault`.
fn failed(fault: Fault) -> RuntimeError {
    RuntimeError::new(fault.to_string())
}

impl Maker {
    /// Records the sidecar of the start `spec` and makes everything of its lease but the
    /// start. A failure after the sidecar was recorded answers the sidecar, for the worker
    /// to take apart what was made; one before answers none, and nothing is taken apart: a
    /// sidecar of the name that is there already is another lease's.
    ///
    /// An image in the store with no kept config is removed and fetched again. That deletes
    /// the root such an image gives as the lower layer of a container's mounts, so it relies
    /// on every container of an earlier daemon that still runs on it pinning it (an adopted
    /// run's pin stays until its removal, and a pinned image is not removed), and on the
    /// start-up pass that removes the rest of what an earlier agent left (P12b) having run
    /// before the first run is made. The removal and a fetch of the same image never overlap
    /// (the image store holds one gate for each digest).
    ///
    /// A host job is refused unless the runtime launches them (`CrunSettings::host_jobs`),
    /// and when it names a mount, run files or a GPU; it is not wired, since it has the
    /// host's network.
    fn make(&self, spec: &RunSpec, head: Head) -> Result<Run, Unmade> {
        let Head {
            name,
            host,
            given,
            args,
            image,
            gpu,
            assembled,
            labels,
            entry,
            mut marks,
        } = head;
        // Before the first thing the sidecar names is made (see the module `ledger`), the
        // image's pin among them: the next start removes a pin a dead process left, once it
        // has removed the lease's container.
        self.ledger
            .record(&entry)
            .map_err(|why| Fault::step("record the lease's labels", &why))
            .map_err(|fault| (fault, None))?;
        self.mark(&mut marks, "recorded");
        let made = self.make_recorded(
            spec,
            (&given, &image),
            (&args, assembled.as_ref()),
            (&name, gpu),
            &mut marks,
        );
        let (container, lease, cgroup): (ContainerSpec, LeaseDirs, LeaseCgroup) =
            made.map_err(|fault| (fault, Some(Box::new(entry.clone()))))?;
        let net = match host {
            // The host's own network: nothing to wire, nothing to forward.
            true => net::Net::asked(js_types::JobNetwork::None, Vec::new(), Vec::new()),
            false => net::Net::asked(spec.network, spec.ports.clone(), spec.host_ports.clone()),
        };
        Ok(Run {
            spec: container,
            labels,
            lease,
            cgroup,
            cap: js_contain::hard_memory_max(spec.memory_bytes),
            backstop: None,
            state: State::Created,
            net,
            entry,
            logs: RunLogs::asked(self.logs.as_ref(), spec.log_sink.as_ref()),
            fs: assembled.map(|made| rootfs::RunFs::new(made.sources)),
            marks,
        })
    }

    /// Starts the made container of `run` and wires its network; see
    /// [`CrunRuntime::start_staged`]. The sidecar is rewritten once, in the state running
    /// with the supervisor, after the network is wired.
    fn start(&self, run: &mut Run) -> Result<(), RuntimeError> {
        let _entered = self.rt.enter();
        let now = (self.hooks.now_ms)();
        let started = self
            .oci
            .start_supervised(&run.spec, run.logs.file_cap(self.logs.as_ref()))
            .map_err(|why| RuntimeError::new(format!("start crun: {why}")))?;
        run.marks.push(("supervised", (self.hooks.now_ms)()));
        // The capture made both files before the supervisor said it was ready.
        let files = (stdout_path(&run.lease.dir), stderr_path(&run.lease.dir));
        run.logs
            .start(self.logs.as_ref(), (&files.0, &files.1), &run.lease.dir);
        run.backstop = run.cgroup.parent_ooms().ok();
        let ports = (self.egress.as_deref(), self.forwarder.as_ref());
        let wired = run
            .net
            .attach(ports, &self.oci, (&run.spec, &started), self.hooks.now_ms);
        // The sidecar says the run runs from here on: a later start adopts it. A failure to
        // write it leaves the run started, for the caller to remove.
        run.entry = run.entry.running(now, Supervisor::of(started.id()));
        let recorded = self.ledger.rewrite(&run.entry);
        run.marks.push(("running", (self.hooks.now_ms)()));
        run.state = State::Running {
            run: started,
            since_ms: now,
        };
        wired?;
        recorded.map_err(|why| RuntimeError::new(format!("record the running lease: {why}")))
    }
}
