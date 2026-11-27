//! The Seatbelt profile of a native run: the policy the agent's flags and the run's start
//! give, as the text and the parameters the supervisor applies in the job's own process
//! (`sandbox_init_with_parameters`, before it starts the program).

use std::path::{Path, PathBuf};

use js_agent_wire::NetworkLevel;
use js_mac_pool::seatbelt::{
    AbsPath, Assurance, Caps, MountGrant, MountTrees, Narrowing, Net, Policy, Profile, ReadGrant,
    ReadModel, SlotLayout,
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

/// Root's home, spelled as the kernel resolves it (`/var` is a link to `/private/var`).
pub(super) const ROOT_HOME: &str = "/private/var/root";

/// Where the data volume is mounted. Every path of the data volume also has a spelling
/// below it (`/Users` is a firmlink to `/System/Volumes/Data/Users`), and which spelling
/// Seatbelt matches was not measured, so a homes root is denied under both, as the privacy
/// fence's places are (`native/fence.rs`).
pub(super) const DATA_VOLUME: &str = "/System/Volumes/Data";

/// The roots below which every run's profile denies reads but for the run's own home (the
/// account's home, which holds its run directory):
/// `homes`, where the pool makes its accounts' homes (`/Users` on a host, the one value
/// `--native-homes` takes), and root's home, each spelled as given and below
/// [`DATA_VOLUME`]. A home of another user is readable to everyone when its modes say so
/// (`0755` is a common mode of a home), so its modes alone do not keep a job out.
pub(super) fn home_roots(homes: &Path) -> Vec<PathBuf> {
    [homes, Path::new(ROOT_HOME)]
        .iter()
        .flat_map(|root| {
            let data = format!("{DATA_VOLUME}{}", root.display());
            [root.to_path_buf(), PathBuf::from(data)]
        })
        .collect()
}

/// The run directory of the account whose home is `home`.
pub(super) fn run_dir(home: &Path) -> PathBuf {
    home.join(RUN_DIR)
}

/// The level of a run's profile: what its start asks ([`net_of`]), met with the agent's
/// ceiling and, for a run that pins a bundle, with the bundle's `network_max`. A job of the
/// level `none` gets loopback under every ceiling but `off`, which leaves it no network.
pub(super) fn level(config: &NativeConfig, asked: NetworkLevel, bundle: Option<Net>) -> Net {
    let level = config.network.net().narrow(net_of(asked));
    match bundle {
        Some(cap) => level.narrow(cap),
        None => level,
    }
}

/// What a run's profile reads by (`native/allow.rs`, `NativeRuntime::reads_of`).
#[derive(Debug)]
pub(super) enum Reads {
    /// The open model: reads are allowed but for the read denies.
    Open,
    /// The allow model: the base set of the assured macOS version, the run's own places, its
    /// mounts and these grants.
    Allow(Assurance, Vec<ReadGrant>),
}

/// The profile of a run at the level `net` ([`level`]) under the policy of `config`, run as an account whose home is
/// `home`: writes are granted to the run directory (`<home>/exec`) and to the grants in
/// `mounts` (the run's job mounts and its account's own per-user temporary and cache folders),
/// and nowhere else, `read_only` (the run's files directory, the `dest`
/// of each input below `exec/` and the link of each read-only mount, each named for an error)
/// is denied writes, `mounts` are granted after the read denies (every mount root is one),
/// and `trees` are the shared trees of the run, if any. `reads` is the read model of the
/// run: under [`Reads::Allow`] the policy is given its grants with the assurance, and a run
/// with shared trees is refused (the trees' rules are the open model's).
///
/// The read denies are the agent's (`config.read_denies`), the places only the privacy
/// controls guard among them ([`fence::PRIVACY_ONLY_PLACES`]); a mount at or below one of
/// those is refused, since its grant would follow the deny and reopen it. Reads below the
/// homes roots ([`home_roots`] of `config.homes`) are denied too, but for the account's own
/// home `home`, which the profile grants back for reading (it writes only below `exec/`).
///
/// # Errors
///
/// A path the profile cannot name, a mount in a fenced place, or a policy or profile that
/// cannot be built.
pub(super) fn build(
    config: &NativeConfig,
    net: &Net,
    home: &Path,
    (read_only, mounts): (&[(&str, PathBuf)], &[MountGrant]),
    (trees, reads): (Option<MountTrees>, Reads),
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
    // The policy never grants more than the ceiling: `net` was met with it already
    // ([`level`]), so the cap lowers nothing here; it is a defence in depth.
    let read_model = match &reads {
        Reads::Open => ReadModel::Open,
        Reads::Allow(..) => ReadModel::Allow,
    };
    let caps = Caps {
        net: config.network.net(),
        read_model,
    };
    let deny_homes = home_roots(&config.homes)
        .iter()
        .map(|root| AbsPath::new(root).map_err(|why| fail(&root.display(), &why)))
        .collect::<Result<Vec<AbsPath>, RuntimeError>>()?;
    let narrowing = Narrowing {
        net: Some(net.clone()),
        deny_homes,
        ..Narrowing::default()
    };
    let policy = Policy::base(layout, caps)
        .and_then(|policy| match reads {
            Reads::Open => Ok(policy),
            Reads::Allow(assurance, grants) => policy.allowing(&assurance, grants),
        })
        .and_then(|policy| policy.narrowed(narrowing))
        .map_err(|why| fail(&"the policy", &why))?;
    Profile::for_policy(&policy).map_err(|why| fail(&"the profile", &why))
}

/// The network level of a start as the profile's. `off`, the job level `none`, is loopback, as
/// a Linux job of that level has its own (the ceiling `off` lowers it to no network). `on` and
/// `open` are one profile, with no network rule: the packet filter, which only `on` requires,
/// is what tells them apart.
fn net_of(level: NetworkLevel) -> Net {
    match level {
        NetworkLevel::Off => Net::Loopback,
        NetworkLevel::Loopback => Net::Loopback,
        NetworkLevel::On | NetworkLevel::Open => Net::On,
    }
}

#[cfg(test)]
mod tests;
