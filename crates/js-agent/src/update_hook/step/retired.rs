//! The root helper job of the releases before the one-program package (`js-agent.session`, run
//! by `js-mac-session`): the package no longer carries it, and a package cannot remove a file an
//! earlier package laid down, so the hook does, on a launchd host.
//!
//! `retire`, in every launchd `cutover` that has not issued the start, after the agent is
//! stopped and before the pf step and the agent's start:
//!
//! 1. the job is booted out (nothing happens when launchd has it not loaded), so that no
//!    helper keeps running beside the new agent;
//! 2. its plist, [`Env::helper_plist`], is removed (one that is not there is no error), so that a
//!    reboot starts no helper.
//!
//! Both are done again by a rerun that finds the start not issued, and each may be: a job that is
//! not loaded is not booted out, and a plist that is gone is not removed. A boot out or a
//! removal that fails fails the step (`stop_failed`, class environment) with the agent's start
//! not issued. The stale programs `bin/js-mac-session` and `bin/js-mac-provision` are left on
//! disk: the plists of a previous release name them, and a rollback starts that release.
//!
//! `restore`, in a launchd `rollback` that starts the previous release's agent, only when that
//! release's tree carries the helper's plist (`tree/Library/LaunchDaemons/js-agent.session.plist`):
//! the job is booted out, the plist is put back at [`Env::helper_plist`] when nothing is there
//! (written beside it and renamed over it, mode 0644), bootstrapped from the previous release's
//! tree and waited for as the agent is, so that the previous agent, which dials the helper,
//! finds it. A previous release without the plist has no helper to restore and the host is
//! untouched.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::update_hook::{Done, Held, LAUNCHCTL, Manager, Params, Reason, Record};

use super::Env;
use super::cutover::{bring_up_unit, environment};

/// The launchd label of the root helper job that releases before the one-program package ran.
pub const RETIRED_HELPER: &str = "js-agent.session";

/// Where the helper's plist is on a Mac: the one an earlier package laid down.
pub const HELPER_PLIST: &str = "/Library/LaunchDaemons/js-agent.session.plist";

/// The mode of the helper's plist when `restore` puts it back.
const PLIST_MODE: u32 = 0o644;

/// Boots the retired helper out and removes its plist; nothing on a host that is not under
/// launchd.
pub(super) fn retire(env: &Env<'_>) -> Result<(), Done> {
    if env.manager != Manager::Launchd {
        return Ok(());
    }
    stop(env)?;
    env.ports.point();
    match fs::remove_file(env.helper_plist) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => Err(environment(
            Reason::StopFailed,
            format!("cannot remove {}: {why}", env.helper_plist.display()),
        )),
        _ => Ok(()),
    }
}

/// Boots the retired helper out, puts its plist back and starts it from the release directory
/// `previous`, when that release carries the plist; nothing otherwise.
pub(super) fn restore(
    env: &Env<'_>,
    held: &Held,
    record: &mut Record,
    params: &Params,
    previous: &Path,
) -> Result<(), Done> {
    let shipped = Manager::Launchd.path_in(previous, RETIRED_HELPER);
    if env.manager != Manager::Launchd || !shipped.is_file() {
        return Ok(());
    }
    stop(env)?;
    put_back(env, &shipped)?;
    let plist_arg = shipped.to_string_lossy();
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

/// Boots the helper out, after a kill point.
fn stop(env: &Env<'_>) -> Result<(), Done> {
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

/// Copies `shipped` to [`Env::helper_plist`] unless something is there already: written beside
/// it and renamed over it, so that a launchd that reads the directory finds the whole file.
fn put_back(env: &Env<'_>, shipped: &Path) -> Result<(), Done> {
    let target = env.helper_plist;
    if fs::symlink_metadata(target).is_ok() {
        return Ok(());
    }
    let mut name = target.as_os_str().to_owned();
    name.push(".new");
    let beside = PathBuf::from(name);
    env.ports.point();
    fs::read(shipped)
        .and_then(|bytes| fs::write(&beside, bytes))
        .and_then(|()| fs::set_permissions(&beside, Permissions::from_mode(PLIST_MODE)))
        .and_then(|()| fs::rename(&beside, target))
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot put {} back: {why}", target.display()),
            )
        })
}
