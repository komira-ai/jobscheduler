//! Starting the job half of the agent: from the checked flags and the hello, the worker
//! that owns the lifecycle, and how a drain treats it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::facts::Platform;
use crate::lifecycle::{Capacity, Config, Local};
use crate::quota::ScratchQuota;
use crate::readiness::WorkerHook;
use crate::session::Drain;
use crate::system::{Backend, Machine};
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
    /// The scratch quota to bound the runs with: given exactly when `--scratch-quota` is,
    /// after its probe passed.
    pub quota: Option<Arc<dyn ScratchQuota>>,
}

/// Starts the worker when `--container-bin` or `--native-helper-socket` is given, and
/// `None` when neither is: an agent without them registers and heartbeats and runs no job.
/// The worker has built the lifecycle and adopted the containers an earlier process left
/// when this returns, and the lines it has to say (the engine's warnings, what it adopted)
/// are written to `err`.
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
    let quota = seams.quota.map(|quota| local.run_quota(quota));
    let Local {
        mounts,
        scratch,
        files,
        files_warning: _,
        scratch_quota: _,
        scratch_default_gib: _,
        scratch_max_inodes: _,
        scratch_quota_failures: _,
        scratch_usage_wait_secs: _,
        scratch_abandon_limit: _,
        container,
        native,
        state,
        watch,
        stop_grace,
        drain_wait,
    } = local;
    // `Local` holds at most one of the two.
    let backend = match (container, native) {
        (Some(container), _) => Backend::Container(container),
        (None, Some(native)) => Backend::Native(native),
        (None, None) => return Ok(None),
    };
    let flag = match &backend {
        Backend::Container(_) => "--container-bin",
        Backend::Native(_) => "--native-helper-socket",
    };
    let (Some(scratch), Some(state)) = (scratch, state) else {
        return Ok(None);
    };
    let config = Config {
        capacity: Capacity::of(hello),
        allow_host_jobs: false,
        mounts,
        scratch,
        files,
        state,
        stop_grace,
        quota,
    };
    let outbox = Arc::new(Outbox::new());
    let worker = Worker::start(Plan {
        config,
        backend,
        host_id: hello.host_id.clone(),
        platform: platform.clone(),
        watch,
        machine: seams.machine.clone(),
        step_every: STEP_EVERY,
        usage: Some(outbox.clone()),
        rescue_bound: seams.rescue_bound,
        readiness: seams.readiness,
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
