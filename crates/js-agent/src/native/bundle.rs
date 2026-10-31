//! The bundle of a native run: the trees it pins, the copies the run may write to, and
//! what the program, the arguments and the environment become once the bundle's rules are
//! applied.
//!
//! A start that pins a bundle (`RunSpec::bundle`) goes through two steps, both before the
//! program starts:
//!
//! 1. [`pin`], before the run's user exists: the trees of the bundle are pinned for the
//!    run's lease, so the store cannot evict one while the run uses it, and an entrypoint
//!    the program names (`@bundle/<name>`) must be one the bundle has. A start that fails
//!    here has made nothing.
//! 2. [`prepare`], after the user exists and before anything of the user's runs: the
//!    config's writable trees are cloned into the run's own `trees` directory (owned by the
//!    user, private to them), the config is resolved for the run (`js_bundle::materialise`:
//!    where each tree is, the environment, the arguments), and the entrypoint is resolved
//!    to the file it names, which must lie inside its tree and be a program.
//!
//! The pins end with the run: [`Pinned`] unpins when it is dropped, whichever way the start
//! or the run ends. The clones are inside the user's home, which the helper sweeps when it
//! deletes the user.
//!
//! The profile of a run that pins a bundle is the same as any native run's: reads are open
//! (`ReadModel::Open`, the only model with a profile builder), so the store's trees need no
//! grant. The network is the lowest of what the start asks, what the bundle's tools need
//! (`seatbelt.network_max`) and what the agent allows.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;

use js_agent_wire::{BUNDLE_PROGRAM_PREFIX, NativeSpec, NetworkLevel};
use js_bundle::{BundleConfig, Inputs, MaterialiseError, Materialised, materialise};
use js_treestore::Owner;

use crate::bundle::{Pinned, PlaceError, Placer};

/// Why a bundle could not be made ready for a run.
#[derive(Debug, thiserror::Error)]
pub(super) enum Prepare {
    /// The store refused the pin or a clone.
    #[error("{0}")]
    Place(#[from] PlaceError),
    /// A directory could not be made or a path could not be read.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The config does not resolve for this run.
    #[error("{0}")]
    Resolve(#[from] MaterialiseError),
    /// The program names an entrypoint the bundle does not have.
    #[error("the bundle has no entrypoint {0}")]
    NoEntrypoint(String),
    /// The user's ids cannot own a clone.
    #[error("the run's user has ids that cannot own a copy")]
    Owner,
    /// The entrypoint's path is not text.
    #[error("the entrypoint {0} is not a path the profile can name")]
    NotText(String),
    /// The entrypoint leads out of its tree.
    #[error("the entrypoint {0} leads out of its tree")]
    Outside(String),
    /// The entrypoint is not a file that can run.
    #[error("the entrypoint {0} is not an executable file")]
    NotRunnable(String),
}

/// A bundle, ready for a run: what the program is and what it is given.
pub(super) struct Prepared {
    /// The program: the entrypoint's file, or the start's own program when it is not a
    /// bundle's.
    pub program: String,
    /// The arguments, with the bundle's placeholders resolved.
    pub args: Vec<String>,
    /// The environment: the agent's, the bundle's rules, then the job's.
    pub env: Vec<(String, String)>,
    /// The network the run gets, before the agent's own cap.
    pub network: NetworkLevel,
}

/// Pins the bundle `pin` for the run of `lease`. `entrypoint` is the name the start's
/// program asked for, if it asked for one.
pub(super) fn pin(
    placer: &Arc<Placer>,
    pin: &str,
    lease: &str,
    entrypoint: Option<&str>,
) -> Result<Pinned, Prepare> {
    let pinned = placer.pin(pin, lease)?;
    match entrypoint {
        Some(name) if !pinned.config().entrypoints.contains_key(name) => {
            Err(Prepare::NoEntrypoint(name.to_owned()))
        }
        _ => Ok(pinned),
    }
}

/// Makes the pinned bundle ready for the run of `native` as `uid`, whose home is `home`:
/// clones what the bundle says is written to into `<home>/trees` and resolves the config.
/// `tmp` is the run's `TMPDIR`.
pub(super) fn prepare(
    pinned: &Pinned,
    native: &NativeSpec,
    home: &Path,
    (uid, tmp): (u32, &Path),
) -> Result<Prepared, Prepare> {
    let under = home.join("trees");
    super::make_dir(&under, uid)?;
    let gid = fs::metadata(home)?.gid();
    let owner = Owner::new(uid, gid).ok_or(Prepare::Owner)?;
    let writable = pinned.clone_writable(&under, owner)?;
    let base_env = [("TMPDIR".to_owned(), tmp.display().to_string())];
    let inputs = Inputs {
        store_root: pinned.root(),
        writable: &writable,
        developer_dir: None,
        base_env: &base_env,
        job_env: &native.env,
        job_args: &native.args,
    };
    let made = materialise(pinned.config(), &inputs)?;
    let program = match native.program.strip_prefix(BUNDLE_PROGRAM_PREFIX) {
        Some(name) => entrypoint_file(pinned.config(), &made, name)?,
        None => native.program.clone(),
    };
    let network = lower(native.network, pinned.config());
    Ok(Prepared {
        program,
        args: made.args,
        env: made.env,
        network,
    })
}

/// The file the entrypoint `name` resolves to, as text. It is what the path leads to, links
/// followed: that must be a file of the entrypoint's own tree that can run. A store whose
/// trees were changed under the agent (a link put where a program was) is refused here and
/// not run.
fn entrypoint_file(
    config: &BundleConfig,
    made: &Materialised,
    name: &str,
) -> Result<String, Prepare> {
    // `pin` has checked that the bundle has the entrypoint and the config that every
    // entrypoint is below a tree of its own, so these cannot fail; the errors are not
    // closures, which nothing would ever call.
    let path = made
        .entrypoints
        .get(name)
        .ok_or(Prepare::NoEntrypoint(name.to_owned()))?;
    let tree = config
        .entrypoints
        .get(name)
        .and_then(|entrypoint| entrypoint.path.trees().next())
        .ok_or(Prepare::NoEntrypoint(name.to_owned()))?;
    let dir = made
        .trees
        .get(tree)
        .ok_or(Prepare::NoEntrypoint(name.to_owned()))?;
    let (file, root) = (fs::canonicalize(path)?, fs::canonicalize(dir)?);
    if !file.starts_with(&root) {
        return Err(Prepare::Outside(name.to_owned()));
    }
    let meta = fs::metadata(&file)?;
    if !(meta.is_file() && meta.permissions().mode() & 0o111 != 0) {
        return Err(Prepare::NotRunnable(name.to_owned()));
    }
    let text = file.to_str().ok_or(Prepare::NotText(name.to_owned()))?;
    Ok(text.to_owned())
}

/// How much network a level is, lowest first.
fn rank(level: NetworkLevel) -> u8 {
    match level {
        NetworkLevel::Off => 0,
        NetworkLevel::Loopback => 1,
        NetworkLevel::On => 2,
    }
}

/// The lower of what the start asks and what the bundle's tools need.
fn lower(asked: NetworkLevel, config: &BundleConfig) -> NetworkLevel {
    let needed = match config.seatbelt.network_max {
        js_bundle::NetworkLevel::Off => NetworkLevel::Off,
        js_bundle::NetworkLevel::Loopback => NetworkLevel::Loopback,
        js_bundle::NetworkLevel::On => NetworkLevel::On,
    };
    match rank(asked) <= rank(needed) {
        true => asked,
        false => needed,
    }
}
