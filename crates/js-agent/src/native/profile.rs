//! The command line of a native run: the agent's own start mode (`__job-start`) with the
//! Seatbelt profile built from the agent's policy, then the program and its arguments, and the
//! self-test that runs it once at the agent's start.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use js_agent_wire::{NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{
    AbsPath, Caps, MountGrant, Narrowing, Net, Policy, Profile, SlotLayout,
};
use js_mac_session::disclaim::Program;

use super::{NativeConfig, fail};
use crate::lifecycle::RuntimeError;

/// The run directory below a pool user's home: the working directory and the only place a
/// job may write (the profile grants writes below it and nowhere else in the home).
pub(super) const RUN_DIR: &str = "exec";

/// The job's `HOME` (and `CFFIXED_USER_HOME`), below the run directory. A tool that keeps
/// preferences or caches under `~/Library` makes them here, so the account's real home is
/// never written and no `~/Library` is ever made in it. It is a directory of its own, not
/// the run directory itself, so the working directory holds only what the job puts there.
pub(super) const JOB_HOME_DIR: &str = ".home";

/// The job's `TMPDIR`, below the run directory.
pub(super) const JOB_TMP_DIR: &str = ".tmp";

/// The directory of a bundle's writable tree clones, below the run directory.
pub(super) const JOB_TREES_DIR: &str = ".trees";

/// The program that sets the job's `HOME`: it runs inside the profile, as the first thing
/// after the agent's start mode applies it, and executes the job's program.
const ENV_PROGRAM: &str = "/usr/bin/env";

/// The run directory of the pool user whose home is `home`.
pub(super) fn run_dir(home: &Path) -> PathBuf {
    home.join(RUN_DIR)
}

/// The command line of `native` under the policy of `config`, run as a user whose home is
/// `home`: the agent itself (`config.start`) in its `__job-start` mode with the profile and
/// its parameters, then `/usr/bin/env HOME=<job home>` and the program and its arguments. The
/// profile grants writes to the run directory (`<home>/exec`) and not to `home`.
///
/// No shell is in the command line. The helper sets the run's limits in the child before it
/// executes the agent, and the agent applies the profile in its own process before it
/// executes anything else, so nothing the job's environment steers runs outside the profile
/// and the environment does not enter the command line at all.
///
/// # Errors
///
/// The policy or the profile cannot be built, `config.start` or `native.program` is not an
/// absolute path.
/// The environment is not read here: [`NativeRuntime`] checks it before it makes a user.
pub fn profile_argv(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
) -> Result<Vec<String>, RuntimeError> {
    argv_of(config, native, home, (&[], &[]))
}

/// [`profile_argv`] for a run whose files and inputs are at `read_only` (its files directory,
/// the `dest` of each input below `exec/` and the link of each read-only mount): the profile
/// denies writes there. Reads are open under the only read model that has a profile
/// ([`ReadModel::Open`]), and the agent refuses at start a files root that a read deny
/// covers, so the run reads them. `mounts` are the run's job mounts, granted after the read
/// denies (every mount root is one), so the run reads and, for a read-write one, writes
/// only the host directories it was given (`native/mounts.rs`).
pub(super) fn argv_of(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
    grants: (&[(&str, PathBuf)], &[MountGrant]),
) -> Result<Vec<String>, RuntimeError> {
    let (profile, command, start) = confinement_of(config, native, home, grants)?;
    let none = String::new();
    let (program, args) = command.split_first().unwrap_or((&none, &[]));
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    Ok(profile
        .start_argv(&start, program, &args)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect())
}

/// The profile of [`argv_of`], the command it confines (`/usr/bin/env HOME=<job home>
/// <program> <args>`) and the start program (`config.start`): what the start mode is given,
/// for a start that applies the profile itself (`native/one_program.rs`).
pub(super) fn confinement_of(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
    (read_only, mounts): (&[(&str, PathBuf)], &[MountGrant]),
) -> Result<(Profile, Vec<String>, AbsPath), RuntimeError> {
    let run_path = run_dir(home);
    let run = AbsPath::new(&run_path).map_err(|why| fail(&run_path.display(), &why))?;
    let inputs = read_only
        .iter()
        .map(|(what, path)| AbsPath::new(path).map_err(|why| fail(what, &why)))
        .collect::<Result<Vec<AbsPath>, RuntimeError>>()?;
    let layout = SlotLayout {
        trees: None,
        run,
        uid_entry: None,
        inputs,
        read_denies: config.read_denies.clone(),
        mounts: mounts.to_vec(),
    };
    // The policy never grants more than the ceiling. That is a defence in depth only: a
    // start above the ceiling was refused by `check_native` before its user existed, so this
    // narrowing lowers nothing the agent admitted. What can still lower the level is the
    // bundle's `network_max` (`bundle::lower`), applied to `native` before this.
    let caps = Caps {
        net: config.network.net(),
        read_model: config.read_model,
    };
    let narrowing = Narrowing {
        net: Some(net_of(native.network)),
        ..Narrowing::default()
    };
    let policy = Policy::base(layout, caps)
        .and_then(|policy| policy.narrowed(narrowing))
        .map_err(|why| fail(&"the policy", &why))?;
    let profile = Profile::for_policy(&policy).map_err(|why| fail(&"the profile", &why))?;
    // The helper sets `HOME` to the account's home whatever the request says (and that
    // is left as it is: the helper's wire does not change), so the job's `HOME` is set
    // here, inside the profile, by `env`, which executes the program. The value is a path
    // of the run, which every user may read on a command line; no secret goes here.
    AbsPath::new(Path::new(&native.program)).map_err(|why| fail(&native.program, &why))?;
    AbsPath::new(Path::new(ENV_PROGRAM)).map_err(|why| fail(&ENV_PROGRAM, &why))?;
    let start = AbsPath::new(&config.start).map_err(|why| fail(&config.start.display(), &why))?;
    let job_home = run_path.join(JOB_HOME_DIR);
    let command: Vec<String> = [
        ENV_PROGRAM.to_owned(),
        format!("HOME={}", job_home.display()),
        native.program.clone(),
    ]
    .into_iter()
    .chain(native.args.iter().cloned())
    .collect();
    Ok((profile, command, start))
}

/// Runs `/usr/bin/true` under the profile of `config` through `config.start`, as the agent:
/// whether the agent's own start mode applies the profile this runtime builds. An agent that
/// fails it must not say `exec = sandbox-exec`, or every job it takes would fail.
///
/// # Errors
///
/// What failed, for the operator: the profile cannot be built, the start program cannot be
/// run, or it refused the profile.
pub fn self_test(config: &NativeConfig) -> Result<(), String> {
    let probe = NativeSpec {
        program: "/usr/bin/true".to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    };
    let argv = profile_argv(config, &probe, &config.homes).map_err(|why| why.0)?;
    let name = config.start.display();
    pool_users_can_run(&config.start).map_err(|why| format!("{name}: {why}"))?;
    let out = Program::new(&argv[0], &argv[1..])
        .env_clear()
        .output()
        .map_err(|why| format!("{name}: {why}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "{name} refused the profile ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Whether a pool user can run `start`: it and every directory above it is readable and
/// searchable by others. The self-test runs as the agent, which can run anything, while the
/// jobs run as pool users.
fn pool_users_can_run(start: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let others = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o005 == 0o005)
            .map_err(|why| why.to_string())
    };
    for path in start.ancestors() {
        if !others(path)? {
            return Err(format!(
                "{} is not readable and searchable by the pool users, who run every job through it",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The network level of a start as the profile's. `on` and `open` are one profile, with no
/// network rule: the packet filter, which only `on` requires, is what tells them apart.
fn net_of(level: NetworkLevel) -> Net {
    match level {
        NetworkLevel::Off => Net::Off,
        NetworkLevel::Loopback => Net::Loopback,
        NetworkLevel::On | NetworkLevel::Open => Net::On,
    }
}

#[cfg(test)]
mod tests;
