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
//!   below the root (through `..` or a link). It is the directory the provider answered for
//!   this run, `<root>/mount-provider/<provider>/<run>/<index>` ([`PROVIDER_DIR`],
//!   [`is_runs_tree`]), never another run's or a folder above one. A mount above the root is
//!   refused, since its grant would open every run's trees. With a trees root, every run's
//!   profile carries the root's rules, with no tree when the run mounts none, so a run
//!   without a tree reads nothing below the root.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use js_agent_wire::{JobMount, MountMode, MountSource, refusal};
use js_mac_pool::seatbelt::{AbsPath, MountTrees, PathError, TreesError};

use crate::lifecycle::{MountRoots, NativeRefusal};

/// The flag that names the trees root.
pub const TREES_ROOT_FLAG: &str = "--native-trees-root";

/// The folder below the trees root in which the provider makes each run's trees:
/// `<root>/mount-provider/<provider, hex>/<run, hex>/<index>`.
pub const PROVIDER_DIR: &str = "mount-provider";

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
/// absolute `given`, which is a `--mount-root-ro` and no `--mount-root`, with no other mount
/// root above or below it. `None` when it is
/// not given.
///
/// # Errors
///
/// The text names the flag: a path that is not absolute, no `--mount-provider`, a path that
/// cannot be resolved, one that is no mount root, one that is a writable root, and one that
/// lies above or below another mount root.
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
    // Every other root is a read deny, rendered after the trees' grants: one above the trees
    // root, or below it, would hide the trees it covers.
    if let Some(other) = roots
        .paths()
        .find(|path| *path != root && (root.starts_with(path) || path.starts_with(&root)))
    {
        return Err(format!(
            "{TREES_ROOT_FLAG}: {} and the mount root {} hold one another: every other mount \
             root is a read deny of every run, which would hide the trees",
            root.display(),
            other.display()
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

/// Whether `dir` (canonical) is a tree the provider answered for the run `run_id` below the
/// trees root `root`: `<root>/mount-provider/<provider>/<run>/<index>`, where `<provider>` is
/// lowercase hex, `<run>` is the hex of the name the agent gives the run in its provider
/// calls (`js.<run_id>`) and `<index>` is a number.
#[must_use]
pub fn is_runs_tree(root: &Path, run_id: &str, dir: &Path) -> bool {
    let Ok(below) = dir.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<&str> = below
        .components()
        .map(|part| part.as_os_str().to_str().unwrap_or_default())
        .collect();
    let run: String = crate::provider::run_name(run_id)
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // A path's components are never empty, so `all` alone says it.
    let hex = |text: &str| {
        text.bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    let number = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
    matches!(parts.as_slice(), [dir, provider, named, index]
        if *dir == PROVIDER_DIR && hex(provider) && *named == run && number(index))
}

/// The trees of the run `run_id`'s mounts below the trees root `root` (canonical): every
/// tree of the run, none when it mounts none (its profile then reads nothing below the root).
/// Each plan is `(dir, read_only, no_exec)`; `dir` is resolved first. A plan whose directory
/// is below or at the root, as given or once resolved, is a tree, and must be strictly below
/// the root once resolved, the run's own ([`is_runs_tree`]), read-only and `no_exec`. Any
/// other plan is not a tree, and is refused when it holds the root.
///
/// # Errors
///
/// `input-unavailable` for a directory that cannot be resolved (one outside the root that
/// is not there is passed over: it is no tree); `input-not-allowed` for a
/// tree that is the root, one that resolves outside it, one that is not a tree the provider
/// answered for this run, one mounted read-write or without `no_exec`, and a mount that holds
/// the root. The text names the directory as given.
pub fn tree_rules(
    root: &Path,
    run_id: &str,
    plans: &[(PathBuf, bool, bool)],
) -> Result<TreeRules, NativeRefusal> {
    let refused = |dir: &Path, why: String| NativeRefusal {
        code: refusal::INPUT_NOT_ALLOWED,
        why: format!("{}: {why}", dir.display()),
    };
    let mut trees = Vec::new();
    for (dir, read_only, no_exec) in plans {
        let canonical = match fs::canonicalize(dir) {
            Ok(canonical) => canonical,
            // A directory outside the root that is not there yet is no tree, and cannot hold
            // the root: a put target, which the agent makes at the commit, is one.
            Err(why) if why.kind() == io::ErrorKind::NotFound && !dir.starts_with(root) => {
                continue;
            }
            Err(why) => {
                return Err(NativeRefusal {
                    code: refusal::INPUT_UNAVAILABLE,
                    why: format!("{}: {why}", dir.display()),
                });
            }
        };
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
        if !is_runs_tree(root, run_id, &canonical) {
            return Err(refused(
                dir,
                format!(
                    "below the trees root {}, a job mounts only a tree the provider answered \
                     for its own run ({PROVIDER_DIR}/<provider>/<run>/<index>)",
                    root.display()
                ),
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
    Ok(TreeRules {
        root: root.to_path_buf(),
        trees,
    })
}

#[cfg(test)]
mod tests;
