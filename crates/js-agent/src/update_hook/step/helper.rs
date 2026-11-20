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
//! or done, and so are those that name a part of a Mac's host setup (`socket_group`,
//! `scratch_root`; see `host_setup`).

use std::path::Path;

use crate::update_hook::{
    Done, HELPER_UNIT_KEY, Held, LAUNCHCTL, Manager, Params, Reason, Record, SCRATCH_ROOT_KEY,
    SOCKET_GROUP_KEY,
};

use super::cutover::{bring_up_unit, environment};
use super::{Env, failure};

/// Refuses a params table that names a helper, or a part of a Mac's host setup, on a host that
/// is not under launchd: the first such key, in the order `helper_unit`, `socket_group`,
/// `scratch_root`.
pub(super) fn refuse_without_launchd(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    if env.manager == Manager::Launchd {
        return Ok(());
    }
    let named = [
        (
            HELPER_UNIT_KEY,
            params.settings.helper_unit.clone(),
            "only a launchd host has a helper job",
        ),
        (
            SOCKET_GROUP_KEY,
            params.settings.socket_group.clone(),
            "only a launchd host's setup makes the group",
        ),
        (
            SCRATCH_ROOT_KEY,
            params
                .scratch_root
                .as_ref()
                .map(|root| root.display().to_string()),
            "only a launchd host's setup gives it a volume",
        ),
    ];
    match named
        .into_iter()
        .find_map(|(key, value, why)| value.map(|value| (key, value, why)))
    {
        Some((key, value, why)) => Err(failure(
            Reason::BadParams,
            format!("{key} {value:?} is set, and {why}"),
        )),
        None => Ok(()),
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
