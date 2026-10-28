//! Starting the job half of the agent: from the checked flags and the hello, the worker
//! that owns the lifecycle, and how a drain treats it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Hello;

use crate::facts::Platform;
use crate::lifecycle::{Capacity, Config, Local};
use crate::session::Drain;
use crate::system::Machine;
use crate::worker::{Handle, Plan, STEP_EVERY, Worker};

/// How long after it stops the jobs a drain goes on heartbeating, on top of the stop
/// grace, so their results reach the server and are acknowledged.
pub const DRAIN_SETTLE: Duration = Duration::from_secs(30);

/// Starts the worker when `--container-bin` is given, and `None` when it is not: an agent
/// without it registers and heartbeats and runs no job. The worker has built the
/// lifecycle and adopted the containers an earlier process left when this returns, and
/// the lines it has to say (the engine's warnings, what it adopted) are written to `err`.
/// `hook` is handed a [`Handle`] to the worker. `rescue_bound` replaces the bound of the
/// rescue (see [`crate::Rescue`]); it is `None` outside a test.
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
    (machine, hook, rescue_bound): (&Arc<dyn Machine>, Option<&dyn Fn(Handle)>, Option<Duration>),
    err: &mut dyn Write,
) -> Result<Option<(Worker, Drain)>, String> {
    let Local {
        mounts,
        scratch,
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
        state,
        stop_grace,
    };
    let worker = Worker::start(Plan {
        config,
        settings,
        host_id: hello.host_id.clone(),
        platform: platform.clone(),
        watch,
        machine: machine.clone(),
        step_every: STEP_EVERY,
        rescue_bound,
    })
    .map_err(|why| format!("--container-bin: {why}"))?;
    worker.take_lines().iter().for_each(|line| {
        let _ = writeln!(err, "js-agent: {line}");
    });
    hook.iter().for_each(|hook| hook(worker.handle()));
    let settle = stop_grace.saturating_add(DRAIN_SETTLE);
    let drain = Drain {
        wait: drain_wait,
        settle,
    };
    Ok(Some((worker, drain)))
}
