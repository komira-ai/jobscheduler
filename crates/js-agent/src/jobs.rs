//! Starting the job half of the agent: from the checked flags and the hello, the worker
//! that owns the lifecycle, and how a drain treats it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::bundle::Admission;
use crate::facts::Platform;
use crate::lifecycle::{Capacity, Config, Local};
use crate::quota::{Mode, Probe, ScratchQuota};
use crate::readiness::WorkerHook;
use crate::session::Drain;
use crate::system::Machine;
use crate::usage::Outbox;
use crate::worker::{Handle, Plan, STEP_EVERY, Worker};

/// How long after it stops the jobs a drain goes on heartbeating, on top of the stop
/// grace, so their results reach the server and are acknowledged.
pub const DRAIN_SETTLE: Duration = Duration::from_secs(30);

/// What the worker is made over, besides the flags.
pub struct Seams<'a> {
    /// The clock, the runtime and the host view.
    pub machine: &'a Arc<dyn Machine>,
    /// Handed a [`Handle`] to the worker once it has started.
    pub hook: Option<&'a dyn Fn(Handle)>,
    /// The bound of the rescue in place of [`crate::RESCUE_BOUND`]; `None` outside a test.
    pub rescue_bound: Option<Duration>,
    /// The readiness hook the worker runs for a whole-node lease; `None` without
    /// `--readiness-hook`.
    pub readiness: Option<WorkerHook>,
    /// The rules for a start that names a bundle; `None` without `--bundle-store`.
    pub bundle: Option<Admission>,
    /// The quota to bound the runs with, and what its probe found when the agent started:
    /// given when `--scratch-quota` is, after its probe passed, and for a runtime that gives
    /// each job a temporary directory (crun), whatever the probe found (a quota whose probe
    /// was not project starts unhealthy, and no run starts until it works).
    pub quota: Option<FoundQuota>,
}

/// The quota of the scratch root the agent found at start, for [`Seams::quota`].
pub struct FoundQuota {
    /// The quota.
    pub quota: Arc<dyn ScratchQuota>,
    /// What its probe found when the agent started.
    pub probe: Probe,
    /// Whether the runtime gives each job a temporary directory for the quota to bound (crun).
    pub bounds_tmp: bool,
}

/// Starts the worker when the runtime of this build is configured (Linux: the crun runtime or
/// `--container-bin`; macOS: `--native-helper-socket`), and `None` when it is not: an agent
/// started with `--no-jobs` registers and heartbeats and runs
/// no job. The worker has built the lifecycle and adopted the containers an earlier process
/// left when this returns, and the lines it has to say (the engine's warnings, what it
/// adopted) are written to `err`.
/// The hook of `seams` is handed a [`Handle`] to the worker. The outbox returned is where
/// the worker publishes the usage of the leases (nothing, without `--actions-cgroup`) and
/// the session takes it from.
///
/// # Errors
///
/// The text names `--container-bin` (the engine is not Docker or Podman or does not
/// answer, the agent cannot see the host, or the containers of an earlier run cannot be
/// listed) or `--native-helper-socket` (the helper does not answer).
pub fn start(
    local: Local,
    hello: &Hello,
    platform: &Platform,
    seams: Seams<'_>,
    err: &mut dyn Write,
) -> Result<Option<(Worker, Drain, Arc<Outbox>)>, String> {
    let quota = seams.quota.map(|found| {
        let FoundQuota {
            quota,
            probe,
            bounds_tmp,
        } = found;
        let quota = local.run_quota(quota, bounds_tmp);
        match probe.mode {
            Mode::Project => quota,
            Mode::None => quota.unusable(probe.reason),
        }
    });
    let Local {
        mounts,
        providers,
        scratch,
        files,
        files_warning: _,
        scratch_quota: _,
        scratch_default_gib: _,
        scratch_max_inodes: _,
        scratch_quota_failures: _,
        scratch_usage_wait_secs: _,
        scratch_abandon_limit: _,
        job_tmp_gib: _,
        backend,
        state,
        watch,
        settle,
        taint_recheck,
        stop_grace,
        drain_wait,
        image_allow: _,
        egress: _,
        silence_stop,
        vm,
        allow_host_jobs,
        // What the probe found, not the flag, decides: the hello says what it found.
        allow_fuse_jobs: _,
    } = local;
    // `Local` holds a state root and a scratch root with a backend.
    let (backend, scratch, state) = match (backend, scratch, state) {
        (Some(backend), Some(scratch), Some(state)) => (backend, scratch, state),
        _ => return Ok(None),
    };
    let flag = backend.flag();
    let config = Config {
        capacity: Capacity::of(hello),
        allow_host_jobs,
        allow_fuse_jobs: hello.capabilities.contains_key(js_caps::FUSE_JOBS),
        mounts,
        scratch,
        files,
        state,
        stop_grace,
        bundle: seams.bundle.clone(),
        quota,
    };
    let outbox = Arc::new(Outbox::new());
    let worker = Worker::start(Plan {
        config,
        backend,
        host_id: hello.host_id.clone(),
        providers,
        platform: platform.clone(),
        watch,
        settle,
        taint_recheck,
        machine: seams.machine.clone(),
        step_every: STEP_EVERY,
        usage: Some(outbox.clone()),
        rescue_bound: seams.rescue_bound,
        readiness: seams.readiness,
        silence_stop: Some(silence_stop),
        vm,
    })
    .map_err(|why| format!("{flag}: {why}"))?;
    worker.take_lines().iter().for_each(|line| {
        let _ = writeln!(err, "js-agent: {line}");
    });
    seams.hook.iter().for_each(|hook| hook(worker.handle()));
    let settle = stop_grace.saturating_add(DRAIN_SETTLE);
    let drain = Drain {
        wait: drain_wait,
        settle,
    };
    Ok(Some((worker, drain, outbox)))
}
