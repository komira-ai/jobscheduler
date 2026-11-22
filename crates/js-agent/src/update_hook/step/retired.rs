//! The pf job an earlier release of the Mac package ran ([`PF_JOB`], `js-agent.pf-pool`): a third
//! launchd job that loaded the pool's pf anchor at load and every 30 seconds. The agent loads
//! the anchor itself now (at its start, before its check, and every 30 seconds after), so no
//! release carries the job, and a host updated from one that did has to lose it.
//!
//! `cutover` retires it, on every launchd cutover (a first install included) that finds the
//! start not issued, after the helper is replaced and before the agent is started:
//!
//! 1. the job is booted out if launchd has it loaded (the unit port's stop does nothing for a
//!    label that is not loaded), so that it does not load the anchor beside the agent's own;
//! 2. its plist, the file the machine names ([`PF_PLIST`] on a Mac), is removed when it is there,
//!    so that a reboot does not load the job again.
//!
//! Both are no-ops once done, so a rerun repeats them and no mark is written. The anchor itself
//! stays in force through the bootout (the rules stay in the kernel), and the new agent loads it
//! again at its start. Either failing fails the step before the agent is started, class
//! environment.
//!
//! `rollback` restores the job when the previous release's tree carries its plist
//! (`tree/Library/LaunchDaemons/<job>.plist`), because that release's agent does not load the
//! anchor and expects the job to. After the helper is replaced and before the agent is started:
//!
//! 1. the job is booted out;
//! 2. the previous release's own `js-mac-provision` (`tree/Library/Application
//!    Support/js-agent/bin/js-mac-provision`) runs `install --flags-file <flags_file>` to its end:
//!    it loads the anchor the host's flags name unless that anchor is in force already, and loads
//!    nothing while the flags file is absent or sets no `--native-network=on`, so a host whose
//!    configuration package has not arrived is not held back;
//! 3. the plist is copied from the tree to where the cutover removed it, mode 0644, when nothing
//!    is there (a file that is there is left as it is), so that a reboot after the rollback loads
//!    the job again; the copy is written beside it and renamed into place, so the path never
//!    holds part of a plist;
//! 4. the job is bootstrapped from the tree's plist.
//!
//! Each is done again by a rerun that finds the previous start not issued, and each may be: the
//! bootout leaves nothing loaded, the install reloads nothing that is in force, the copy is made
//! only when the file is absent, and the bootstrap follows the bootout. Any of them that fails
//! fails the step before the previous agent is started, class environment. A previous release
//! whose tree has no such plist (a release that loads the anchor itself) gets none of these acts.
//! The job is a launchd matter: a systemd host has none, and neither function is called for it.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::update_hook::{Done, LAUNCHCTL, Manager, Params, Reason};

use super::Env;
use super::cutover::environment;

/// The label of the pf job an earlier release of the Mac package ran.
const PF_JOB: &str = "js-agent.pf-pool";

/// The plist of the pf job on a Mac, in the system's launch daemons directory.
pub const PF_PLIST: &str = "/Library/LaunchDaemons/js-agent.pf-pool.plist";

/// The previous release's `js-mac-provision`, in a macOS package's release tree.
const PROVISION_IN_TREE: &str = "tree/Library/Application Support/js-agent/bin/js-mac-provision";

/// The mode of the plist `rollback` puts back: what a launch daemon's plist must be.
const PLIST_MODE: u32 = 0o644;

/// The suffix of the copy `rollback` writes beside the plist before it renames it into place.
const NEW_SUFFIX: &str = ".new";

/// Boots the retired pf job out when launchd has it loaded, and removes its plist
/// (`env.pf_plist`) when it is there.
pub(super) fn retire(env: &Env<'_>) -> Result<(), Done> {
    stop(env)?;
    let path = env.pf_plist;
    if matches!(fs::symlink_metadata(path), Err(why) if why.kind() == io::ErrorKind::NotFound) {
        return Ok(());
    }
    env.ports.point();
    fs::remove_file(path).map_err(|why| {
        environment(
            Reason::StopFailed,
            format!("cannot remove {}: {why}", path.display()),
        )
    })
}

/// Brings the pf job back as the release directory `previous` ran it, when its tree carries the
/// job's plist; nothing otherwise.
pub(super) fn restore(env: &Env<'_>, params: &Params, previous: &Path) -> Result<(), Done> {
    let plist = Manager::Launchd.path_in(previous, PF_JOB);
    if !plist.is_file() {
        return Ok(());
    }
    stop(env)?;
    let program = previous.join(PROVISION_IN_TREE);
    let program_arg = program.to_string_lossy();
    let flags = params.flags_file.to_string_lossy();
    env.ports.point();
    env.runner
        .run(&program_arg, &["install", "--flags-file", &flags])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot load the pf anchor of {flags} with {program_arg}: {why}"),
            )
        })?;
    put_back(env, &plist)?;
    let plist_arg = plist.to_string_lossy();
    env.ports.point();
    env.runner
        .run(LAUNCHCTL, &["bootstrap", "system", &plist_arg])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot bootstrap {PF_JOB} from {plist_arg}: {why}"),
            )
        })?;
    Ok(())
}

/// Boots the pf job out (nothing is done for a label launchd has not loaded).
fn stop(env: &Env<'_>) -> Result<(), Done> {
    env.ports.point();
    env.ports
        .units
        .stop_no_block(PF_JOB)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {PF_JOB}: {why}")))
}

/// Copies the tree's `plist` to `env.pf_plist`, mode 0644, when nothing is there: written beside
/// it under [`NEW_SUFFIX`] and renamed over it, so the path holds either nothing or the whole
/// plist, and a rerun after a kill mid-copy copies again (the copy truncates a partial one).
fn put_back(env: &Env<'_>, plist: &Path) -> Result<(), Done> {
    let path = env.pf_plist;
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    let mut new = path.as_os_str().to_owned();
    new.push(NEW_SUFFIX);
    let new = Path::new(&new);
    env.ports.point();
    fs::copy(plist, new)
        .and_then(|_| fs::set_permissions(new, fs::Permissions::from_mode(PLIST_MODE)))
        .and_then(|()| fs::rename(new, path))
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!(
                    "cannot put {} back at {}: {why}",
                    plist.display(),
                    path.display()
                ),
            )
        })
}
