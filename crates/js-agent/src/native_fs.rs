//! The rules of a native (macOS) job's mounts that need no Mac to decide, built and tested on
//! every platform; the native runtime (`native/mounts.rs`, `native/settings.rs`) and the
//! macOS platform (`platform/macos.rs`) call them.
//!
//! - **A provider's tree is read-only** ([`check_provider_mode`]). Every job that names the
//!   same tree is given the same directory of the provider's mount, so a job may not write
//!   it.
//! - **The trees root** (`--native-trees-root`, [`trees_root`]) is the one mount point under
//!   which the provider serves every run's trees (tierstore's NFS mount of the Mac). It is a
//!   `--mount-root-ro` and needs a `--mount-provider`. It is the one mount root that is not a
//!   read deny of every run ([`denied_roots`]): the run's profile denies reads below it and
//!   grants back only the run's own trees ([`TreeRules`]), and a deny of the root after
//!   those grants would hide every tree.
//! - **A tree is a mount strictly below the trees root** ([`tree_rules`]): it is read-only,
//!   mounted `no_exec`, and neither the root itself nor a path that only spells its way
//!   below the root (through `..` or a link). A mount above the root is refused, since its
//!   grant would open every run's trees.

use std::fs;
use std::path::{Path, PathBuf};

use js_agent_wire::{JobMount, MountMode, MountSource, refusal};
use js_mac_pool::seatbelt::{AbsPath, MountTrees, PathError, TreesError};

use crate::lifecycle::{MountRoots, NativeRefusal};

/// The flag that names the trees root.
pub const TREES_ROOT_FLAG: &str = "--native-trees-root";

/// Refuses a mount of a provider's tree whose mode is not read-only, `input-not-allowed`. A
/// mount of a host path is not this rule's.
///
/// # Errors
///
/// The refusal names the mount's target.
pub fn check_provider_mode(mount: &JobMount) -> Result<(), NativeRefusal> {
    match (&mount.source, mount.mode) {
        (MountSource::Provider { .. }, MountMode::ReadWrite | MountMode::CopyOnWrite) => {
            Err(NativeRefusal {
                code: refusal::INPUT_NOT_ALLOWED,
                why: format!(
                    "{}: a provider's tree is shared by every native job that names it, so a \
                     native job mounts it read-only only",
                    mount.target
                ),
            })
        }
        _ => Ok(()),
    }
}

/// Checks `--native-trees-root` (`given`) against the mount roots and the providers the
/// agent was given (`providers`: at least one `--mount-provider`): the canonical path of an
/// absolute `given`, which is a `--mount-root-ro` and no `--mount-root`. `None` when it is
/// not given.
///
/// # Errors
///
/// The text names the flag: a path that is not absolute, no `--mount-provider`, a path that
/// cannot be resolved, one that is no mount root, and one that is a writable root.
pub fn trees_root(
    given: Option<&Path>,
    roots: &MountRoots,
    providers: bool,
) -> Result<Option<PathBuf>, String> {
    let Some(given) = given else {
        return Ok(None);
    };
    if !given.is_absolute() {
        return Err(format!(
            "{TREES_ROOT_FLAG}: {} is not an absolute path",
            given.display()
        ));
    }
    if !providers {
        return Err(format!(
            "{TREES_ROOT_FLAG}: needs --mount-provider: the trees root is where a provider \
             serves its trees"
        ));
    }
    let root = fs::canonicalize(given)
        .map_err(|why| format!("{TREES_ROOT_FLAG}: {}: {why}", given.display()))?;
    if roots.writable_paths().any(|path| path == root) {
        return Err(format!(
            "{TREES_ROOT_FLAG}: {} is a --mount-root, which jobs may write: the trees root \
             is a --mount-root-ro only",
            root.display()
        ));
    }
    if !roots.paths().any(|path| path == root) {
        return Err(format!(
            "{TREES_ROOT_FLAG}: {} is no --mount-root-ro",
            root.display()
        ));
    }
    Ok(Some(root))
}

/// A root of the agent that jobs may mount from (`--mount-root`, `--mount-root-ro`), by
/// canonical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRoot {
    /// The canonical path of the root.
    pub path: PathBuf,
    /// Jobs may mount from it read-only only (`--mount-root-ro`).
    pub read_only: bool,
    /// It is the trees root (`--native-trees-root`): a mount below it is a tree.
    pub trees: bool,
}

/// The roots of `roots` as the native runtime holds them, by path: one entry per canonical
/// path, writable when any root of that path is, and the trees root marked (`trees`, the
/// canonical path [`trees_root`] answered).
#[must_use]
pub fn roots_of(roots: &MountRoots, trees: Option<&Path>) -> Vec<MountRoot> {
    let writable: Vec<&Path> = roots.writable_paths().collect();
    let mut all: Vec<&Path> = roots.paths().collect();
    all.sort_unstable();
    all.dedup();
    all.into_iter()
        .map(|path| MountRoot {
            path: path.to_path_buf(),
            read_only: !writable.contains(&path),
            trees: trees == Some(path),
        })
        .collect()
}

/// The roots that are read denies of every run: all of `roots` but the trees root, whose
/// reads the run's [`TreeRules`] deny and grant back tree by tree.
pub fn denied_roots(roots: &[MountRoot]) -> impl Iterator<Item = &MountRoot> {
    roots.iter().filter(|root| !root.trees)
}

/// One run's trees below the trees root: what the run's profile reads of the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRules {
    /// The trees root, canonical.
    pub root: PathBuf,
    /// The run's trees, canonical, each strictly below `root`, in the order of the mounts.
    pub trees: Vec<PathBuf>,
}

/// Why [`TreeRules::mount_trees`] cannot name the trees in a profile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeRulesError {
    /// A path a profile cannot name.
    #[error("{path}: {why}")]
    Path {
        /// The path.
        path: String,
        /// What the profile refuses in it.
        why: PathError,
    },
    /// The trees are not a set the profile takes.
    #[error(transparent)]
    Trees(#[from] TreesError),
}

impl TreeRules {
    /// The trees as the profile builder takes them.
    ///
    /// # Errors
    ///
    /// A path the profile cannot name, or the builder's own refusal of the set.
    pub fn mount_trees(&self) -> Result<MountTrees, TreeRulesError> {
        let named = |path: &Path| {
            AbsPath::new(path).map_err(|why| TreeRulesError::Path {
                path: path.display().to_string(),
                why,
            })
        };
        let trees = self
            .trees
            .iter()
            .map(|tree| named(tree))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(MountTrees::new(named(&self.root)?, trees)?)
    }
}

/// The trees of a run's mounts below the trees root `root` (canonical), or `None` when no
/// mount is one. Each plan is `(dir, read_only, no_exec)`; `dir` is resolved first. A plan
/// whose directory is below or at the root, as given or once resolved, is a tree, and must be
/// strictly below the root once resolved, read-only and `no_exec`. Any other plan is not a
/// tree, and is refused when it holds the root.
///
/// # Errors
///
/// `input-unavailable` for a directory that cannot be resolved; `input-not-allowed` for a
/// tree that is the root, one that resolves outside it, one mounted read-write or without
/// `no_exec`, and a mount that holds the root. The text names the directory as given.
pub fn tree_rules(
    root: &Path,
    plans: &[(PathBuf, bool, bool)],
) -> Result<Option<TreeRules>, NativeRefusal> {
    let refused = |dir: &Path, why: String| NativeRefusal {
        code: refusal::INPUT_NOT_ALLOWED,
        why: format!("{}: {why}", dir.display()),
    };
    let mut trees = Vec::new();
    for (dir, read_only, no_exec) in plans {
        let canonical = fs::canonicalize(dir).map_err(|why| NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!("{}: {why}", dir.display()),
        })?;
        if !dir.starts_with(root) && !canonical.starts_with(root) {
            if root.starts_with(&canonical) {
                return Err(refused(
                    dir,
                    format!(
                        "holds the trees root {}: its grant would open every run's trees",
                        root.display()
                    ),
                ));
            }
            continue;
        }
        let tree = canonical.display().to_string();
        if canonical == root {
            return Err(refused(dir, TreesError::IsRoot(tree).to_string()));
        }
        if !canonical.starts_with(root) {
            let root = root.display().to_string();
            return Err(refused(
                dir,
                TreesError::NotBelow { tree, root }.to_string(),
            ));
        }
        if !read_only {
            return Err(refused(
                dir,
                format!(
                    "a tree below the trees root {} is mounted read-only only",
                    root.display()
                ),
            ));
        }
        if !no_exec {
            return Err(refused(
                dir,
                format!(
                    "a tree below the trees root {} is mounted no_exec only: the profile \
                     grants a tree reads only",
                    root.display()
                ),
            ));
        }
        trees.push(canonical);
    }
    Ok((!trees.is_empty()).then(|| TreeRules {
        root: root.to_path_buf(),
        trees,
    }))
}

#[cfg(test)]
mod tests;
