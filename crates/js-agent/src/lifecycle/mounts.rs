//! The roots a job may mount from, named in the agent's own configuration, and the
//! resolution of a message's [`Mount`] against them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use js_agent_wire::Mount;

use super::runtime::ResolvedMount;

/// Why a mount was not resolved. The wire code is the same for all three
/// ([`js_agent_wire::refusal::INVALID_MOUNT`]); the variant is for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MountError {
    /// The root name is not one the agent was given.
    #[error("the mount names a root this agent does not have")]
    UnknownRoot,
    /// The path does not exist below the root, or cannot be read.
    #[error("the mount path cannot be resolved below its root")]
    Unresolvable,
    /// The path leads out of the root, by `..`, an absolute part or a symbolic link.
    #[error("the mount path leaves its root")]
    Escapes,
}

/// The mount roots of this agent: names to canonical directories.
///
/// A root is canonicalised once, when the agent starts. A mount is then resolved by
/// joining its path to the root, canonicalising the result (which follows every symbolic
/// link on the way) and requiring that it is still below the root: a link or a `..` that
/// leads out is refused whatever the wire validator already said about the text. The
/// canonical path is what the runtime mounts, never the message's text.
///
/// A path that is swapped for a link between the resolution and the runtime's mount is
/// not defended against: the roots are directories the operator owns, and anyone who can
/// write inside one can already change what a job sees.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountRoots {
    roots: BTreeMap<String, PathBuf>,
}

impl MountRoots {
    /// The roots of `entries`, as `(name, path)` pairs, each path canonicalised.
    ///
    /// # Errors
    ///
    /// A name the wire would not accept in a mount, a name given twice, or a path that is
    /// not an existing directory. The text names the offending entry.
    pub fn new(entries: &[(String, PathBuf)]) -> Result<Self, String> {
        let mut roots = BTreeMap::new();
        for (name, path) in entries {
            let probe = Mount {
                root_name: name.clone(),
                rel: "x".to_owned(),
                ro: true,
            };
            probe
                .validate()
                .map_err(|_| format!("--mount-root {name:?}: not a usable root name"))?;
            let canonical = fs::canonicalize(path)
                .ok()
                .filter(|path| path.is_dir())
                .ok_or_else(|| format!("--mount-root {name}: not an existing directory"))?;
            if roots.insert(name.clone(), canonical).is_some() {
                return Err(format!("--mount-root {name}: given twice"));
            }
        }
        Ok(Self { roots })
    }

    /// The canonical path of the root `name`, when there is one.
    pub fn root(&self, name: &str) -> Option<&Path> {
        self.roots.get(name).map(PathBuf::as_path)
    }

    /// Resolves `mount` below its root.
    ///
    /// # Errors
    ///
    /// See [`MountError`].
    pub fn resolve(&self, mount: &Mount) -> Result<ResolvedMount, MountError> {
        let root = self.root(&mount.root_name).ok_or(MountError::UnknownRoot)?;
        let host = fs::canonicalize(root.join(&mount.rel)).map_err(|_| MountError::Unresolvable)?;
        match host.starts_with(root) {
            true => Ok(ResolvedMount {
                host,
                target: format!("/mnt/{}/{}", mount.root_name, mount.rel),
                ro: mount.ro,
            }),
            false => Err(MountError::Escapes),
        }
    }
}
