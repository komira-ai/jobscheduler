//! The job that keeps the pool's pf anchor loaded on a Mac (`pf_unit` of the params): a second
//! launchd job of the same package, `js-agent __pf-anchor --flags-file <flags_file>` at load and
//! every 30 seconds. The agent does not start with `--native-network on` unless that anchor is
//! in force, and the job's first run after a bootstrap races the agent's start, so `cutover` loads
//! the anchor itself before the agent starts:
//!
//! 1. the job is booted out (a run under way ends first), so the plist the staged release carries
//!    is the one launchd loads next;
//! 2. the staged release's own agent
//!    (`tree/Library/Application Support/js-agent/bin/js-agent`) runs
//!    `__pf-anchor --flags-file <flags_file>` to its end: it loads the anchor the host's flags
//!    name unless that anchor is in force already, and loads nothing while the flags file is
//!    absent or sets no `--native-network=on`, so a host whose configuration package has not
//!    arrived is not held back;
//! 3. the job's plist is bootstrapped from the staged release's tree,
//!    `tree/Library/LaunchDaemons/<pf_unit>.plist`; its own runs then keep the anchor, read the
//!    flags file afresh each time, and launchd loads it again at every boot.
//!
//! All three are done again by a rerun that finds the start not issued, and each may be: the
//! bootout leaves nothing loaded, the install reloads nothing that is in force, and the bootstrap
//! follows the bootout. Any of them that fails fails the step before the agent is started, class
//! environment, with the agent's start not issued.
//!
//! `rollback` replaces the job with the previous release's when the previous release's tree
//! carries its plist (`restore`): the job is booted out and the previous tree's plist is
//! bootstrapped, and no anchor is installed: the anchor in force stays loaded through a bootout,
//! and the previous release's own program runs at load. Without that, a job loaded from the
//! staged release would be left running the agent's `__pf-anchor` verb, which the previous
//! release's agent does not have. A previous release whose tree carries no such plist leaves the
//! job as it is. A host whose params name no job is untouched. The job is a launchd matter: the
//! params of a systemd host that name one are refused by every step (`bad_params`) before
//! anything is read or done.

use std::path::Path;

use js_mac_provision::daemon::PF_ANCHOR_VERB;

use crate::update_hook::{Done, LAUNCHCTL, Manager, Params, Reason, agent_in};

use super::cutover::environment;
use super::{Env, failure};

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
/// `release`'s agent, and bootstraps the job that release carries; nothing when the params name
/// no pf job.
pub(super) fn replace(env: &Env<'_>, params: &Params, release: &Path) -> Result<(), Done> {
    let Some(job) = params.settings.pf_unit.as_deref() else {
        return Ok(());
    };
    stop(env, job)?;
    let program = agent_in(Manager::Launchd, release);
    let program_arg = program.to_string_lossy();
    let flags = params.flags_file.to_string_lossy();
    env.ports.point();
    env.runner
        .run(&program_arg, &[PF_ANCHOR_VERB, "--flags-file", &flags])
        .map_err(|why| {
            environment(
                Reason::StartFailed,
                format!("cannot load the pf anchor of {flags} with {program_arg}: {why}"),
            )
        })?;
    bootstrap(env, job, release)
}

/// Boots the pf job out and bootstraps the one the release directory `release` carries, loading
/// no anchor; nothing when the params name no pf job or `release` carries no plist for it.
pub(super) fn restore(env: &Env<'_>, params: &Params, release: &Path) -> Result<(), Done> {
    let Some(job) = params.settings.pf_unit.as_deref() else {
        return Ok(());
    };
    if !Manager::Launchd.path_in(release, job).is_file() {
        return Ok(());
    }
    stop(env, job)?;
    bootstrap(env, job, release)
}

/// Boots `job` out, after a kill point.
fn stop(env: &Env<'_>, job: &str) -> Result<(), Done> {
    env.ports.point();
    env.ports
        .units
        .stop_no_block(job)
        .map_err(|why| environment(Reason::StopFailed, format!("cannot stop {job}: {why}")))
}

/// Bootstraps `job` from the plist the release directory `release` carries, after a kill point.
fn bootstrap(env: &Env<'_>, job: &str, release: &Path) -> Result<(), Done> {
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
        })
        .map(drop)
}
