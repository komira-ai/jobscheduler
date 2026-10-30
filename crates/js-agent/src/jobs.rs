//! Starting the job half of the agent: from the checked flags and the hello, the worker
//! that owns the lifecycle, and how a drain treats it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::bundle::Admission;
use crate::facts::Platform;
use crate::lifecycle::{Capacity, Config, Local};
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
}

/// Starts the worker when `--container-bin` is given, and `None` when it is not: an agent
/// without it registers and heartbeats and runs no job. The worker has built the
/// lifecycle and adopted the containers an earlier process left when this returns, and
/// the lines it has to say (the engine's warnings, what it adopted) are written to `err`.
/// The hook of `seams` is handed a [`Handle`] to the worker. The outbox returned is where
/// the worker publishes the usage of the leases (nothing, without `--actions-cgroup`) and
/// the session takes it from.
///
/// # Errors
///
/// The text names `--container-bin`: the engine is not Docker or Podman or does not
/// answer, the agent cannot see the host, or the containers of an earlier run cannot be
/// listed.
pub fn start(
    local: Local,
    hello: &Hello,
    platform: &Platform,
    seams: Seams<'_>,
    err: &mut dyn Write,
) -> Result<Option<(Worker, Drain, Arc<Outbox>)>, String> {
    let Local {
        mounts,
        scratch,
        files,
        files_warning: _,
        scratch_quota: _,
        container,
        state,
        watch,
        stop_grace,
        drain_wait,
    } = local;
    let (Some(settings), Some(scratch), Some(state)) = (container, scratch, state) else {
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
        bundle: seams.bundle.clone(),
    };
    let outbox = Arc::new(Outbox::new());
    let worker = Worker::start(Plan {
        config,
        settings,
        host_id: hello.host_id.clone(),
        platform: platform.clone(),
        watch,
        machine: seams.machine.clone(),
        step_every: STEP_EVERY,
        usage: Some(outbox.clone()),
        rescue_bound: seams.rescue_bound,
        readiness: seams.readiness,
    })
    .map_err(|why| format!("--container-bin: {why}"))?;
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
