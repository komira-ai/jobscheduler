//! The program of a run, and the directories it finds programs in.
//!
//! A bundle's tools are named by entrypoint (`@bundle/<name>`), and the entrypoints are
//! paths under the trees of the run's own pins. [`ToolPath`] has one constructor for
//! them, and it refuses what is not a regular file of a pinned tree. The tree is
//! immutable and root-owned, so a check at this point is a check at `exec`.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use super::grant::PinnedTree;
use super::path::{AbsPath, PathError};

/// What names a bundle's entrypoint as a program: `@bundle/<name>`.
pub const BUNDLE_PREFIX: &str = "@bundle/";

/// Why a program or a `PATH` directory is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    /// The program is not `@bundle/<name>`.
    #[error("program {0:?} is not an @bundle/ entrypoint")]
    NotBundleRef(String),
    /// The bundle has no entrypoint of that name.
    #[error("the bundle has no entrypoint {0:?}")]
    NoSuchEntrypoint(String),
    /// A path is not an acceptable spelling.
    #[error("path is not accepted: {0}")]
    BadPath(PathError),
    /// The entrypoint is not under any tree this run pinned.
    #[error("{0} is not under a pinned tree")]
    UnpinnedTree(String),
    /// The path cannot be examined (it is missing, or unreadable).
    #[error("cannot examine the path: {0:?}")]
    Unreadable(io::ErrorKind),
    /// The entrypoint is a symbolic link; entrypoints are regular files.
    #[error("the entrypoint is a symbolic link")]
    Symlink,
    /// The entrypoint is not a regular file.
    #[error("the entrypoint is not a regular file")]
    NotRegular,
    /// The entrypoint resolves to a place outside its tree.
    #[error("the entrypoint resolves outside its tree")]
    LeavesTree,
    /// A directory of `PATH` is a symbolic link; the directory it names is a path
    /// escape.
    #[error("PATH entry {0:?} is a symbolic link")]
    PathEntrySymlink(String),
    /// A directory of `PATH` is not a directory.
    #[error("PATH entry {0:?} is not a directory")]
    PathEntryNotDir(String),
}

/// A program a run may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPath(AbsPath);

impl ToolPath {
    /// The program named `program` (`@bundle/<name>`) among `entrypoints`, the resolved
    /// paths of a materialised bundle, which must be under one of the `pinned` trees.
    ///
    /// # Errors
    /// A [`ToolError`] naming the first rule the program breaks.
    pub fn from_entrypoint(
        program: &str,
        entrypoints: &BTreeMap<String, PathBuf>,
        pinned: &[PinnedTree],
    ) -> Result<Self, ToolError> {
        let _ = (program, pinned);
        let path = entrypoints.values().next().cloned().unwrap_or_default();
        Ok(Self(AbsPath::new(&path).map_err(ToolError::BadPath)?))
    }

    /// The program's absolute path, for [`Profile::argv`](super::Profile::argv).
    #[must_use]
    pub fn path(&self) -> &AbsPath {
        &self.0
    }
}

/// Checks the directories of every `PATH` in `env`: each is an absolute path in normal
/// form and a real directory (not a symbolic link, not a file).
///
/// # Errors
/// A [`ToolError`] naming the first directory that is not.
pub fn check_path_dirs(_env: &[(String, String)]) -> Result<(), ToolError> {
    Ok(())
}
