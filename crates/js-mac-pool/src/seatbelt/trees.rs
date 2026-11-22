//! The trees a run reads from a shared mount: its own, and nothing else below the mount's
//! root.

use std::collections::BTreeSet;

use super::path::AbsPath;
use super::policy::MAX_LISTED;

/// Why a set of trees is refused. Each error names the path it is about.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreesError {
    /// A tree is not below the mount's root.
    #[error("the tree {tree} is not below the mount root {root}")]
    NotBelow {
        /// The tree's path.
        tree: String,
        /// The mount's root.
        root: String,
    },
    /// A tree is the mount's root itself: it would grant every tree on the mount.
    #[error("the tree {0} is the mount root itself")]
    IsRoot(String),
    /// More than [`MAX_LISTED`] trees.
    #[error("too many trees")]
    TooMany,
}

/// The trees of one run on a shared mount (the store's read-only mount of every job's
/// trees). The profile denies reads below `root`, then allows reads in each tree and the
/// metadata of each folder between the root and a tree (a path walk to the tree, and
/// `realpath` or `getcwd` inside it, need it), so a run reads its own trees and cannot read
/// or list another run's.
///
/// Every path is canonical, as for [`MountGrant`](super::MountGrant): Seatbelt matches the
/// resolved path, so a link in the run directory to a tree is covered by the tree's grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountTrees {
    root: AbsPath,
    trees: BTreeSet<AbsPath>,
}

impl MountTrees {
    /// The trees `trees` below the mount root `root`. No trees is allowed: nothing below
    /// the root is readable then.
    ///
    /// # Errors
    /// [`TreesError::IsRoot`] for a tree that is `root`, [`TreesError::NotBelow`] for one
    /// that is not below it (by whole components), and [`TreesError::TooMany`] for more
    /// than [`MAX_LISTED`] distinct trees.
    pub fn new(root: AbsPath, trees: Vec<AbsPath>) -> Result<Self, TreesError> {
        for tree in &trees {
            if tree == &root {
                return Err(TreesError::IsRoot(tree.as_str().to_owned()));
            }
            if !tree.starts_with(&root) {
                return Err(TreesError::NotBelow {
                    tree: tree.as_str().to_owned(),
                    root: root.as_str().to_owned(),
                });
            }
        }
        let trees: BTreeSet<AbsPath> = trees.into_iter().collect();
        if trees.len() > MAX_LISTED {
            return Err(TreesError::TooMany);
        }
        Ok(Self { root, trees })
    }

    /// The mount's root.
    pub(super) fn root(&self) -> &AbsPath {
        &self.root
    }

    /// The trees, in path order.
    pub(super) fn trees(&self) -> &BTreeSet<AbsPath> {
        &self.trees
    }
}
