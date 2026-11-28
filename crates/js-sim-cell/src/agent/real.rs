//! The real agent behind the wire: the lifecycle of `js-agent`
//! ([`js_agent::lifecycle::Lifecycle`]) over the in-memory runtime of
//! [`js_conformance::agent::memory`], with a clock whose time is the kernel's.
//!
//! The process hands it each heartbeat reply whole ([`RealAgent::apply`]) and builds each
//! heartbeat from what it holds ([`RealAgent::held_jobs`], [`RealAgent::retired_jobs`]); it
//! takes a step on every tick, as the real agent does. A job ends when the process's end
//! timer fires: its container exits with status 0 ([`RealAgent::run_ends`]), and the
//! lifecycle reports the result at its next step. What the agent runs is read from the
//! runtime.
//!
//! The lifecycle is set up as an agent with no host jobs, no mounts, no run files, no
//! bundle store, no scratch quota and no post-job checks, offering the host's capacity
//! with GPU devices `0..gpus`. Its scratch and state roots are directories of their own
//! under the root the [`super::AgentKind::Real`] names, made private (mode `0700`, as the
//! lifecycle demands) and removed with the process. A new process gets new ones and a new
//! runtime: it holds nothing, finds no container and remembers no retired run (design
//! section 8, rows 4 and 31, for a host whose containers died with the agent).
//!
//! It is for small host counts: each process writes its held list into its state root at
//! every step. The fake is the agent a cell runs by the hundred.
//!
//! The lifecycle books GPU devices by index, and a job that asks for GPUs names the indices
//! `0..gpus`, so on a host of more than one GPU a second job that asks for one is refused
//! `no-capacity` where the fake runs it. The cell's hosts have one GPU. The cell writes
//! down what the lifecycle does with the per-job messages in `real_jobs.rs`, as it does for
//! the fake.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use js_agent::lifecycle::{
    Capacity, Config, Lifecycle, MountRoots, NoChecks, ScratchRoot, StateRoot, hold,
};
use js_agent_wire::{HeartbeatReply, HeldJob};
use js_conformance::agent::memory::{Memory, Time};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, Resources};

/// How long a stopped container gets before it is killed; the in-memory runtime ends it at
/// once.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// The status a run that ends by itself exits with.
const SUCCEEDED: i32 = 0;

/// One real agent: the lifecycle, the runtime it drives, its clock, and its directories.
pub(crate) struct RealAgent {
    life: Lifecycle,
    runtime: Memory,
    time: Time,
    /// The scratch and state roots, removed after the lifecycle that uses them is gone.
    dirs: (TestDir, TestDir),
}

impl RealAgent {
    /// A fresh agent for `host`, on a host of `capacity`, its roots made under `root`;
    /// `None` when a root cannot be made private or opened, or the lifecycle does not
    /// start.
    ///
    /// # Panics
    ///
    /// When a root cannot be made at all, as [`TestDir::under`].
    pub(crate) fn new(root: &Path, host: &HostId, capacity: &Resources) -> Option<Self> {
        let scratch = private_dir(root, &format!("real-{host}-scratch"));
        let state = private_dir(root, &format!("real-{host}-state"));
        scratch
            .zip(state)
            .and_then(|dirs| Self::start(dirs, capacity))
    }

    /// The lifecycle over a new runtime and clock, with `dirs` as its scratch and state
    /// roots.
    fn start(dirs: (TestDir, TestDir), capacity: &Resources) -> Option<Self> {
        let scratch = ScratchRoot::open(dirs.0.path()).ok();
        let state = StateRoot::open(dirs.1.path()).ok();
        let mounts = MountRoots::new(&[]).ok();
        let roots = scratch.zip(state).zip(mounts);
        let config = roots.map(|((scratch, state), mounts)| Config {
            capacity: Capacity {
                cpu_millis: capacity.cpu_millis(),
                memory_bytes: capacity.memory_bytes(),
                disk_bytes: capacity.disk_bytes(),
                gpus: (0..capacity.gpus())
                    .filter_map(|gpu| u32::try_from(gpu).ok())
                    .collect(),
            },
            allow_host_jobs: false,
            allow_fuse_jobs: false,
            mounts,
            scratch,
            files: None,
            state,
            stop_grace: STOP_GRACE,
            bundle: None,
            quota: None,
        });
        let runtime = Memory::default();
        let time = Time::default();
        let adopted = config.and_then(|config| {
            let runtime = Box::new(runtime.clone());
            let clock = Box::new(time.clone());
            Lifecycle::adopt(config, runtime, Box::new(NoChecks), clock).ok()
        });
        adopted.map(|(life, _)| Self {
            life,
            runtime,
            time,
            dirs,
        })
    }

    /// Writes the hold file into the lifecycle's state root, as the drain of an update does
    /// ([`hold::write`]): from its next start on, the lifecycle refuses every start with the
    /// code `host-held` and makes nothing. Returns whether the file was written.
    pub(crate) fn hold(&self) -> bool {
        let root = StateRoot::open(self.dirs.1.path());
        root.is_ok_and(|root| hold::write(&root).is_ok())
    }

    /// The lifecycle's clock reads `now` from here on.
    pub(crate) fn at(&self, now: FarmTime) {
        self.time.0.set(now.as_millis());
    }

    /// The jobs the lifecycle holds, for a heartbeat's `jobs`, each with its result once it
    /// has ended.
    pub(crate) fn held_jobs(&self) -> Vec<HeldJob> {
        self.life.held_jobs()
    }

    /// The ids of the stops of jobs the lifecycle did not hold, not yet acknowledged, for a
    /// heartbeat's `retired_jobs`.
    pub(crate) fn retired_jobs(&self) -> Vec<String> {
        self.life.retired_jobs()
    }

    /// The ids of the jobs whose containers run now, read from the runtime.
    pub(crate) fn running(&self) -> BTreeSet<String> {
        self.runtime.running()
    }

    /// Lets the lifecycle make progress, as the real agent does on every tick.
    pub(crate) fn step(&mut self) {
        self.life.step();
    }

    /// Hands `reply` to the lifecycle and lets it take a step; returns the job id of each
    /// container it created meanwhile, in order.
    pub(crate) fn apply(&mut self, reply: &HeartbeatReply) -> Vec<String> {
        let before = self.runtime.created().len();
        self.life.apply(reply);
        self.life.step();
        self.runtime.created().into_iter().skip(before).collect()
    }

    /// The job's own time ran out: when its container still runs it exits with status 0,
    /// and the lifecycle takes a step. Returns whether it still ran.
    pub(crate) fn run_ends(&mut self, job: &str) -> bool {
        let ran = self.runtime.exit(job, SUCCEEDED);
        self.life.step();
        ran
    }
}

/// A new directory under `root`, named after `name`, that only its owner may use; `None`
/// when its mode cannot be set. Panics when it cannot be made, as [`TestDir::under`].
fn private_dir(root: &Path, name: &str) -> Option<TestDir> {
    let dir = TestDir::under(root, name);
    let private = fs::Permissions::from_mode(0o700);
    fs::set_permissions(dir.path(), private).ok().map(|()| dir)
}
