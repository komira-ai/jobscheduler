//! The root helper job that releases before the one-program agent carried (`js-agent.session`,
//! a second launchd job beside the agent's, with the program `js-mac-session` and the socket the
//! agent dialled). The package no longer carries the job, its plist or its program, and the
//! agent dials nothing, so the steps of a launchd host deal with the helper's leftovers and
//! nothing more:
//!
//! - `cutover` retires the job before the agent is started: the job is booted out if launchd has
//!   it loaded (the agent is down already, so nothing holds its socket), and then its plist,
//!   [`RETIRED_HELPER_PLIST`], is removed from the system's launch daemons directory, so that a
//!   reboot does not start a helper whose program is gone. A job that is not loaded and a plist
//!   that is not there are what a host without the helper shows, and are not failures. Either act
//!   failing fails the step before the agent is started (`stop_failed`, class environment), and
//!   a rerun does both again, each a no-op once done. A first install retires too: a Mac that
//!   ran an earlier install of the agent may hold the job;
//! - `rollback` gives a release that still carried the helper its helper again: when the previous
//!   release's tree holds `tree/Library/LaunchDaemons/js-agent.session.plist`, the job is booted
//!   out, the plist is bootstrapped from that tree and the job is started and waited for as the
//!   agent is, before the agent's start; a previous release whose tree holds no such plist has
//!   no helper and nothing is done for it.
//!
//! The helper is a launchd matter: a systemd host does neither.

use std::fs;
use std::io;
use std::path::Path;

use crate::update_hook::{Done, Held, LAUNCHCTL, Manager, Params, Reason, Record};

use super::Env;
use super::cutover::{bring_up_unit, environment};

/// The launchd label of the retired root helper job.
pub const RETIRED_HELPER: &str = "js-agent.session";

/// The retired helper job's plist in the system's launch daemons directory, where launchd reads
/// it at boot.
pub const RETIRED_HELPER_PLIST: &str = "/Library/LaunchDaemons/js-agent.session.plist";

/// Boots the helper job out if launchd has it loaded (the unit port's stop of a job that is not
/// loaded is a no-op).
fn boot_out(env: &Env<'_>) -> Result<(), Done> {
    env.ports.point();
    env.ports
        .units
        .stop_no_block(RETIRED_HELPER)
        .map_err(|why| {
            environment(
                Reason::StopFailed,
                format!("cannot stop {RETIRED_HELPER}: {why}"),
            )
        })
}

/// Retires the helper job on a launchd host: boots it out and removes its plist from the system's
/// launch daemons directory (`Env::retired_helper_plist`); nothing on a systemd host.
pub(super) fn retire(env: &Env<'_>) -> Result<(), Done> {
    if env.manager != Manager::Launchd {
        return Ok(());
    }
    boot_out(env)?;
    let plist = env.retired_helper_plist;
    env.ports.point();
    match fs::remove_file(plist) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => Err(environment(
            Reason::StopFailed,
            format!("cannot remove {}: {why}", plist.display()),
        )),
        _ => Ok(()),
    }
}

/// Gives the previous release its helper job again on a launchd host: boots the job out,
/// bootstraps the plist the previous release directory `previous` carries and starts the job;
/// nothing when that release carries no such plist, and nothing on a systemd host.
pub(super) fn restore(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    previous: &Path,
) -> Result<(), Done> {
    if env.manager != Manager::Launchd {
        return Ok(());
    }
    let plist = Manager::Launchd.path_in(previous, RETIRED_HELPER);
    if !plist.is_file() {
        return Ok(());
    }
    boot_out(env)?;
    let plist_arg = plist.to_string_lossy();
    env.ports.point();
    env.runner
        .run(LAUNCHCTL, &["bootstrap", "system", &plist_arg])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot bootstrap {RETIRED_HELPER} from {plist_arg}: {why}"),
            )
        })?;
    bring_up_unit(env, held, record, params, RETIRED_HELPER, "")
}
