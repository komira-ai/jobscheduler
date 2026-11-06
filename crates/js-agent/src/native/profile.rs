//! The `sandbox-exec` command line of a native run: the Seatbelt profile built from the
//! agent's policy, then the program and its arguments, and the self-test that runs it once
//! at the agent's start.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use js_agent_wire::{NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, Caps, Narrowing, Net, Policy, Profile, SlotLayout};
use js_mac_session::disclaim::Program;

use super::{NativeConfig, fail};
use crate::lifecycle::RuntimeError;

/// The `sandbox-exec` command line of `native` under the policy of `config`, run as a user
/// whose home is `home`: `sandbox-exec`, the profile, the program and its arguments.
///
/// No shell is in the command line. The helper sets the run's limits in the
/// child before it executes `sandbox-exec`, and `sandbox-exec` is the first program of the
/// command, so nothing the job's environment steers runs outside the profile and the
/// environment does not enter the command line at all.
///
/// # Errors
///
/// The policy or the profile cannot be built, or `native.program` is not an absolute path.
/// The environment is not read here: [`NativeRuntime`] checks it before it makes a user.
pub fn profile_argv(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
) -> Result<Vec<String>, RuntimeError> {
    argv_of(config, native, home, &[])
}

/// [`profile_argv`] for a run whose files and inputs are at `read_only` (its files directory
/// and the `dest` of each input below `exec/`): the profile denies writes there. Reads are
/// open under the only read model that has a profile ([`ReadModel::Open`]), and the agent
/// refuses at start a files root that a read deny covers, so the run reads them.
pub(super) fn argv_of(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
    read_only: &[(&str, PathBuf)],
) -> Result<Vec<String>, RuntimeError> {
    let run = AbsPath::new(home).map_err(|why| fail(&home.display(), &why))?;
    let inputs = read_only
        .iter()
        .map(|(what, path)| AbsPath::new(path).map_err(|why| fail(what, &why)))
        .collect::<Result<Vec<AbsPath>, RuntimeError>>()?;
    let layout = SlotLayout {
        run,
        uid_entry: None,
        inputs,
        read_denies: config.read_denies.clone(),
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
    let program =
        AbsPath::new(Path::new(&native.program)).map_err(|why| fail(&native.program, &why))?;
    let args: Vec<OsString> = native.args.iter().map(OsString::from).collect();
    let argv = profile.argv(&program, &args);
    Ok(argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect())
}

/// Runs `/usr/bin/true` under the profile of `config` with `sandbox_exec`, as the agent:
/// whether the Mac has a `sandbox-exec` that accepts the profile this runtime builds. An
/// agent that fails it must not say `exec = sandbox-exec`, or every job it takes would
/// fail.
///
/// # Errors
///
/// What failed, for the operator: the profile cannot be built, the program cannot be run,
/// or it refused the profile.
pub fn self_test(config: &NativeConfig, sandbox_exec: &Path) -> Result<(), String> {
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
    let name = sandbox_exec.display();
    let out = Program::new(sandbox_exec, &argv[1..])
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

/// The network level of a start as the profile's. `on` and `open` are one profile, with no
/// network rule: the packet filter, which only `on` requires, is what tells them apart.
fn net_of(level: NetworkLevel) -> Net {
    match level {
        NetworkLevel::Off => Net::Off,
        NetworkLevel::Loopback => Net::Loopback,
        NetworkLevel::On | NetworkLevel::Open => Net::On,
    }
}
