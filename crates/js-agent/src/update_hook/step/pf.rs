//! The job that keeps the pool's pf anchor loaded on a Mac (`pf_unit` of the params): a third
//! launchd job of the same package, `js-mac-provision install --flags-file <flags_file>` at load
//! and every 30 seconds. The agent does not start with `--native-network on` unless that anchor is
//! in force, and the job's first run after a bootstrap races the agent's start, so `cutover` loads
//! the anchor itself before the agent starts:
//!
//! 1. the job is booted out (a run under way ends first), so the plist the staged release carries
//!    is the one launchd loads next;
//! 2. the staged release's own `js-mac-provision`
//!    (`tree/Library/Application Support/js-agent/bin/js-mac-provision`) runs
//!    `install --flags-file <flags_file>` to its end: it loads the anchor the host's flags name
//!    unless that anchor is in force already, and loads nothing while the flags file is absent or
//!    sets no `--native-network=on`, so a host whose configuration package has not arrived is not
//!    held back;
//! 3. the job's plist is bootstrapped from the staged release's tree,
//!    `tree/Library/LaunchDaemons/<pf_unit>.plist`; its own runs then keep the anchor, read the
//!    flags file afresh each time, and launchd loads it again at every boot.
//!
//! All three are done again by a rerun that finds the start not issued, and each may be: the
//! bootout leaves nothing loaded, the install reloads nothing that is in force, and the bootstrap
//! follows the bootout. Any of them that fails fails the step before the agent is started, class
//! environment, with the agent's start not issued.
//!
//! `rollback` leaves the job as it is: the job's program and flags file are the host's, not a
//! release's (the package's `js-mac-provision` is the one installed now), and the anchor in force
//! stays loaded through a bootout. A host whose params name no job is untouched. The job is a
//! launchd matter: the params of a systemd host that name one are refused by every step
//! (`bad_params`) before anything is read or done.

use std::path::Path;

use crate::update_hook::{Done, LAUNCHCTL, Manager, Params, Reason};

use super::cutover::environment;
use super::{Env, failure};

/// The staged release's `js-mac-provision`, in a macOS package's release tree.
const PROVISION_IN_TREE: &str = "tree/Library/Application Support/js-agent/bin/js-mac-provision";

/// Refuses a params table that names the pf job on a host that is not under launchd.
pub(super) fn refuse_without_launchd(env: &Env<'_>, params: &Params) -> Result<(), Done> {
    match (&params.settings.pf_unit, env.manager) {
        (Some(job), Manager::Systemd) => Err(failure(
            Reason::BadParams,
            format!("pf_unit {job:?} is set, and only a launchd host has a pf job"),
        )),
        _ => Ok(()),
    }
}

/// Boots the pf job out, loads the anchor of the host's flags file with the release directory
/// `release`'s `js-mac-provision`, and bootstraps the job that release carries; nothing when the
/// params name no pf job.
pub(super) fn replace(env: &Env<'_>, params: &Params, release: &Path) -> Result<(), Done> {
    let Some(job) = params.settings.pf_unit.as_deref() else {
        return Ok(());
    };
    env.ports.point();
    env.ports
        .units
        .stop_no_block(job)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {job}: {why}")))?;
    let program = release.join(PROVISION_IN_TREE);
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
    let plist = Manager::Launchd.path_in(release, job);
    let plist_arg = plist.to_string_lossy();
    env.ports.point();
    env.runner
        .run(LAUNCHCTL, &["bootstrap", "system", &plist_arg])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot bootstrap {job} from {plist_arg}: {why}"),
            )
        })?;
    Ok(())
}
