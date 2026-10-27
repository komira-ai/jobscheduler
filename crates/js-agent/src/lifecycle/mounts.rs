//! The roots a job may mount from, named in the agent's own configuration, and the
//! resolution of a message's [`Mount`] against them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use js_agent_wire::Mount;

use super::runtime::ResolvedMount;

/// Why a mount was not resolved. The wire code is the same for all of them
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
    /// The canonical host path holds a character that a runtime's mount option cannot
    /// carry safely: a comma, a colon, a double quote (a value is CSV-quoted with it) or a
    /// control character. A link inside a root can
    /// lead to such a path even though the message's text cannot name one.
    #[error("the mount path holds a character a mount option cannot carry")]
    BadPath,
}

/// The mount roots of this agent: names to canonical directories.
///
/// A root is canonicalised once, when the agent starts. A mount is then resolved by
/// joining its path to the root, canonicalising the result (which follows every symbolic
/// link on the way) and requiring that it is still below the root: a link or a `..` that
/// leads out is refused whatever the wire validator already said about the text. The
/// canonical path is what the runtime mounts, never the message's text.
///
/// The canonical path must also be one a runtime's mount option can carry: no comma, no
/// colon, no double quote, no control character (see [`MountError::BadPath`]). A job with a read-write mount
/// can create such a directory name, and a link to it, inside a root.
///
/// # What this does not defend against
///
/// The resolution is a check at one moment; the runtime binds the path at a later one. A
/// job that holds a read-write mount inside a root can write in that root while another
/// job's mount is being set up: it can replace `shared/x`, checked as a directory of the
/// root, by a link to `/` between the check and the bind. Roots are read-write today, so
/// the window is real, not only the operator's to open. A backend that talks to a
/// container daemon cannot close it by opening the mount by file descriptor: the daemon is
/// another process and binds the path it is given (a `/proc/self/fd` path of the agent
/// means nothing to it). Such a backend re-verifies the mount after the bind, reading back
/// where the daemon bound it ([`CliRuntime`](crate::cli_runtime::CliRuntime) does, and
/// removes a container whose mounts differ). What stays open is the time between the
/// resolution here and the daemon's bind, and a link changed back after the bind: a root
/// shared by jobs of different trust is shared at their risk. A backend that binds in its
/// own process can close it fully by opening the mount with `openat2` and
/// `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`.
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
            if !carries(&canonical) {
                return Err(format!(
                    "--mount-root {name}: the path holds a comma, a colon, a double quote or a control character"
                ));
            }
            if roots.insert(name.clone(), canonical).is_some() {
                return Err(format!("--mount-root {name}: given twice"));
            }
        }
        Ok(Self { roots })
    }

    /// The canonical paths of all the roots.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.roots.values().map(PathBuf::as_path)
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
        if !host.starts_with(root) {
            return Err(MountError::Escapes);
        }
        if !carries(&host) {
            return Err(MountError::BadPath);
        }
        Ok(ResolvedMount {
            host,
            target: format!("/mnt/{}/{}", mount.root_name, mount.rel),
            ro: mount.ro,
        })
    }
}

/// Whether a runtime's mount option can carry `path` as it is: text with no comma (the
/// option separator), no colon (the separator of the short mount form), no double quote
/// (the quote of a value in the option list) and no control character. A path that is
/// not text cannot be.
fn carries(path: &Path) -> bool {
    path.to_str().is_some_and(|text| {
        !text
            .chars()
            .any(|c| matches!(c, ',' | ':' | '"') || c.is_control())
    })
}
