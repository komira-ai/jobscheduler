//! The Seatbelt profile a native job runs under, from the one builder every native run uses
//! (`js_mac_pool::seatbelt`): the job's inputs become a [`Policy`], the policy a [`Profile`], and
//! the profile the C strings the job's own start process applies ([`SandboxInit`]).
//!
//! - The job folder is the run's writable place (the slot layout's run directory, the
//!   parameter `RUN`); nothing else is writable but the devices the builder grants.
//! - Each put directory is a read-write [`MountGrant`].
//! - The trees the provider mounted are the job's [`MountTrees`] below the provider's mount
//!   root: a tree not strictly below the root is refused, naming it ([`TreesError`]).
//! - The network is the job's: none is [`Net::Off`], loopback [`Net::Loopback`], and outbound
//!   ports [`Net::Remote`] (loopback, those ports on any address, and name lookups).
//!
//! There is no per-user folder grant and no rule a run adds for itself. The reads are the
//! builder's open model.
//!
//! [`confine`] makes the C strings too, so a NUL anywhere is refused here, before the agent
//! writes a start file or starts anything.

use std::path::{Path, PathBuf};

use js_mac_pool::seatbelt::{
    AbsPath, BuildError, Caps, InitError, MountGrant, MountTrees, Net, PathError, Policy,
    PolicyError, Ports, PortsError, Profile, ReadModel, SandboxInit, SlotLayout, TreesError,
};
use thiserror::Error;

use super::spec::Network;

/// What a job's profile is made of.
#[derive(Debug, Clone, Copy)]
pub struct Confinement<'a> {
    /// The job folder, absolute and resolved: the one place the job writes.
    pub folder: &'a Path,
    /// The provider's mount root and the paths its trees were mounted at (resolved), when the
    /// job has trees.
    pub mount: Option<(&'a Path, &'a [PathBuf])>,
    /// The job's put directories, absolute and resolved: each is written too.
    pub put_dirs: &'a [PathBuf],
    /// The network the job asked for.
    pub network: &'a Network,
}

/// A job's profile and its text and parameters as C strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confined {
    /// The profile.
    pub profile: Profile,
    /// Its C strings, checked.
    pub init: SandboxInit,
}

/// Why a job's profile cannot be made.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfineError {
    /// A path is not one the builder takes.
    #[error("{what} {path:?}: {why}")]
    Path {
        /// What the path is.
        what: &'static str,
        /// The path, a NUL spelled `\0`.
        path: String,
        /// The builder's reason.
        why: PathError,
    },
    /// The trees are refused; the error names the tree.
    #[error("{0}")]
    Trees(#[from] TreesError),
    /// The outbound ports are refused.
    #[error("outbound ports: {0}")]
    Ports(#[from] PortsError),
    /// The policy is refused.
    #[error("the policy: {0}")]
    Policy(#[from] PolicyError),
    /// The profile cannot be built.
    #[error("the profile: {0}")]
    Build(#[from] BuildError),
    /// A NUL byte in the text or a parameter.
    #[error("the profile: {0}")]
    Init(#[from] InitError),
}

/// The builder's path of `path`, named `what` in the error.
fn abs(what: &'static str, path: &Path) -> Result<AbsPath, ConfineError> {
    AbsPath::new(path).map_err(|why| ConfineError::Path {
        what,
        path: path.to_string_lossy().replace('\0', "\\0"),
        why,
    })
}

/// The network level of a job's request.
fn net_of(network: &Network) -> Result<Net, ConfineError> {
    if !network.outbound_ports.is_empty() {
        return Ok(Net::Remote(Ports::new(&network.outbound_ports)?));
    }
    Ok(if network.loopback {
        Net::Loopback
    } else {
        Net::Off
    })
}

/// The policy of `job`: the builder's open read model, with the network the job asked for as
/// the cap (the job's request is the whole of what it may have).
///
/// # Errors
/// A [`ConfineError`] naming the path, tree or port list the builder refused.
pub fn policy(job: &Confinement<'_>) -> Result<Policy, ConfineError> {
    let trees = match job.mount {
        Some((root, trees)) => {
            let trees = trees
                .iter()
                .map(|tree| abs("a tree", tree))
                .collect::<Result<Vec<_>, _>>()?;
            Some(MountTrees::new(abs("the mount root", root)?, trees)?)
        }
        None => None,
    };
    let mounts = job
        .put_dirs
        .iter()
        .map(|dir| {
            Ok(MountGrant {
                path: abs("a put directory", dir)?,
                write: true,
                no_exec: false,
            })
        })
        .collect::<Result<Vec<_>, ConfineError>>()?;
    let layout = SlotLayout {
        run: abs("the job folder", job.folder)?,
        uid_entry: None,
        inputs: Vec::new(),
        read_denies: Vec::new(),
        mounts,
        trees,
    };
    let caps = Caps {
        net: net_of(job.network)?,
        read_model: ReadModel::Open,
    };
    Ok(Policy::base(layout, caps)?)
}

/// The profile of `job` and its C strings.
///
/// # Errors
/// A [`ConfineError`]: a path or port list the builder refuses, a tree not strictly below the
/// mount root (named), or a NUL byte.
pub fn confine(job: &Confinement<'_>) -> Result<Confined, ConfineError> {
    let profile = Profile::for_policy(&policy(job)?)?;
    let init = profile.sandbox_init()?;
    Ok(Confined { profile, init })
}

#[cfg(test)]
#[path = "confine_tests.rs"]
mod tests;
