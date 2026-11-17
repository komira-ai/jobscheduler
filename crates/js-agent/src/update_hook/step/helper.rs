//! The agent's root helper on a Mac (`helper_unit` of the params): a second launchd job that the
//! agent dials, installed by the same package. The agent's start must find the helper of its own
//! release running, so `cutover` and `rollback` replace it between the agent's stop and the
//! agent's start:
//!
//! 1. the helper is booted out (the agent is down already, so nothing holds its socket);
//! 2. the helper's plist is bootstrapped from the release tree about to run (the staged release
//!    for `cutover`, the previous one for `rollback`), `tree/Library/LaunchDaemons/<helper>.plist`,
//!    and the helper is started (a bootstrap starts a job whose plist says `RunAtLoad`; one that
//!    does not is kickstarted) and waited for as the agent is.
//!
//! A host whose params name no helper is untouched. A helper is a launchd matter: the params of
//! a systemd host that name one are refused by every step (`bad_params`) before anything is read
//! or done.

use std::path::Path;

use crate::update_hook::{Done, Held, LAUNCHCTL, Manager, Params, Reason, Record};

use super::cutover::{bring_up_unit, environment};
use super::{Env, failure};

/// Refuses a params table that names a helper on a host that is not under launchd.
pub(super) fn refuse_without_launchd(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    match (&params.settings.helper_unit, env.manager) {
        (Some(helper), Manager::Systemd) => Err(failure(
            Reason::BadParams,
            format!("helper_unit {helper:?} is set, and only a launchd host has a helper job"),
        )),
        _ => Ok(()),
    }
}

/// Boots the helper out and starts the one the release directory `release` carries; nothing
/// when the params name no helper.
pub(super) fn replace(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    release: &Path,
) -> Result<(), Done> {
    let Some(helper) = params.settings.helper_unit.as_deref() else {
        return Ok(());
    };
    env.ports.point();
    env.ports
        .units
        .stop_no_block(helper)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {helper}: {why}")))?;
    let plist = Manager::Launchd.path_in(release, helper);
    let plist_arg = plist.to_string_lossy();
    env.ports.point();
    env.runner
        .run(LAUNCHCTL, &["bootstrap", "system", &plist_arg])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot bootstrap {helper} from {}: {why}", plist.display()),
            )
        })?;
    bring_up_unit(env, held, record, params, helper)
}
