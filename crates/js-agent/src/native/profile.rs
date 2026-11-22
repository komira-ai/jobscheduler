//! The Seatbelt profile of a native run: the policy the agent's flags and the run's start
//! give, as the text and the parameters the supervisor applies in the job's own process
//! (`sandbox_init_with_parameters`, before it starts the program).

use std::path::{Path, PathBuf};

use js_agent_wire::{NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{
    AbsPath, Caps, MountGrant, MountTrees, Narrowing, Net, Policy, Profile, SlotLayout,
};

use super::{NativeConfig, fail, fence};
use crate::lifecycle::RuntimeError;

/// The run directory below an account's home: the working directory and the only place a
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

/// The run directory of the account whose home is `home`.
pub(super) fn run_dir(home: &Path) -> PathBuf {
    home.join(RUN_DIR)
}

/// The profile of `native` under the policy of `config`, run as an account whose home is
/// `home`: writes are granted to the run directory (`<home>/exec`) and nowhere else, the
/// account's per-user folder (`/var/folders`) is never granted (macOS's own daemons put
/// items there that root may not remove), `read_only` (the run's files directory, the `dest`
/// of each input below `exec/` and the link of each read-only mount, each named for an error)
/// is denied writes, `mounts` are the run's job mounts, granted after the read denies (every
/// mount root is one), and `trees` are the shared trees of the run, if any.
///
/// The read denies are the agent's (`config.read_denies`), the places only the privacy
/// controls guard among them ([`fence::PRIVACY_ONLY_PLACES`]); a mount at or below one of
/// those is refused, since its grant would follow the deny and reopen it.
///
/// # Errors
///
/// A path the profile cannot name, a mount in a fenced place, or a policy or profile that
/// cannot be built.
pub(super) fn build(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
    (read_only, mounts): (&[(&str, PathBuf)], &[MountGrant]),
    trees: Option<MountTrees>,
) -> Result<Profile, RuntimeError> {
    let run_path = run_dir(home);
    let run = AbsPath::new(&run_path).map_err(|why| fail(&run_path.display(), &why))?;
    let inputs = read_only
        .iter()
        .map(|(what, path)| AbsPath::new(path).map_err(|why| fail(what, &why)))
        .collect::<Result<Vec<AbsPath>, RuntimeError>>()?;
    if let Some(fenced) = mounts
        .iter()
        .find(|grant| fence::is_fenced(grant.path.as_str()))
    {
        return Err(fail(
            &"a mount",
            &format!(
                "{} lies in a place only the privacy controls guard",
                fenced.path.as_str()
            ),
        ));
    }
    let layout = SlotLayout {
        trees,
        run,
        uid_entry: None,
        inputs,
        read_denies: config.read_denies.clone(),
        mounts: mounts.to_vec(),
    };
    // The policy never grants more than the ceiling. That is a defence in depth only: a
    // start above the ceiling was refused by `check_native` before its account was lent, so
    // this narrowing lowers nothing the agent admitted. What can still lower the level is the
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
    Profile::for_policy(&policy).map_err(|why| fail(&"the profile", &why))
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
