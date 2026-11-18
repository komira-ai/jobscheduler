//! The config hook on this machine: the steps over the real unit manager and the lease server
//! the host's flags file names.
//!
//! Where the lease server is, and the host id the agent registers under, come from the flags
//! file now in place, read as `js-agent --flags-file` reads it (the same words as the update
//! hook's: see [`Machine`]); the host authenticates by its own certificate and key when the flags
//! file names `--host-cert` and `--host-key` (no bearer), else by the host credential in the
//! agent's state root, else by the token file. Nothing here is an environment variable.

use std::time::Duration;

use js_mac_provision::daemon::AGENT_EXIT_TIMEOUT_SECS;
use js_types::HostId;
use js_update_hook::{Clock, Step, Units};

use crate::facts::SystemCommands;
use crate::own_host::{Lease, RequestLimit};
use crate::update_hook::machine::{connect_with, platform_units};
use crate::update_hook::{FLAGS_FILE_KEY, Machine, MachineLease, StopRunner, SystemClock};

use super::steps::{self, Env};
use super::{Done, Params};

/// The lease server's own-host routes as the flags file now in place says, for the host it
/// names (its own id when the file names none).
///
/// # Errors
/// The flags file cannot be used (see the update hook's `flags_file` errors), or no valid host
/// id can be had. The text starts with the params key, `flags_file`.
fn lease_of(params: &Params, machine: &Machine<'_>) -> Result<Box<dyn Lease>, String> {
    let mut connect = connect_with(
        &params.flags_file,
        &params.state_root,
        &params.token_file,
        machine,
    )?;
    let id = match connect.host_id.take() {
        Some(id) => id,
        None => {
            (machine.own_host_id)().map_err(|why| format!("{FLAGS_FILE_KEY}: no host id: {why}"))?
        }
    };
    HostId::new(&id).map_err(|why| format!("{FLAGS_FILE_KEY}: --host-id: {why}"))?;
    connect.host_id = Some(id);
    Ok(Box::new(MachineLease::connect(
        &connect,
        machine.own_host_id,
        RequestLimit::DEFAULT,
    )))
}

/// Runs `step` over `units` and `clock` and the rest of `machine`.
#[must_use]
pub fn run_on(
    step: Step,
    params: &Params,
    machine: &Machine<'_>,
    units: &dyn Units,
    clock: &dyn Clock,
) -> Done {
    let lease = |params: &Params| lease_of(params, machine);
    let env = Env {
        units,
        clock,
        stat: machine.stat,
        lease: &lease,
    };
    match step {
        Step::Prepare => steps::prepare(&env, params),
        Step::Cutover => steps::cutover(&env, params),
        Step::Verify => steps::verify(&env, params),
        Step::Rollback => steps::rollback(&env, params),
        Step::Cleanup => steps::cleanup(&env, params),
    }
}

/// Runs `step` on this machine: the unit manager of the platform (`systemctl`, or `launchctl`
/// with the kickstart held back while the token file is absent) and the real clock.
#[must_use]
pub fn run_step(step: Step, params: &Params, machine: &Machine<'_>) -> Done {
    let commands = SystemCommands::default();
    let stop = StopRunner::new(Duration::from_secs(u64::from(AGENT_EXIT_TIMEOUT_SECS)));
    let units = platform_units(&commands, &stop, &params.token_file);
    run_on(step, params, machine, &units, &SystemClock)
}
