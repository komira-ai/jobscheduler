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
        let name = program
            .strip_prefix(BUNDLE_PREFIX)
            .ok_or_else(|| ToolError::NotBundleRef(program.to_owned()))?;
        let path = entrypoints
            .get(name)
            .ok_or_else(|| ToolError::NoSuchEntrypoint(name.to_owned()))?;
        let path = AbsPath::new(path).map_err(ToolError::BadPath)?;
        let tree = pinned
            .iter()
            .find(|tree| path.starts_with(tree.path()))
            .ok_or_else(|| ToolError::UnpinnedTree(path.as_str().to_owned()))?;
        let found = std::fs::symlink_metadata(path.as_str())
            .map_err(|e| ToolError::Unreadable(e.kind()))?;
        if found.file_type().is_symlink() {
            return Err(ToolError::Symlink);
        }
        if !found.is_file() {
            return Err(ToolError::NotRegular);
        }
        // A link in a directory above the file can still lead out of the tree. A path
        // that does not resolve is outside, too.
        let real = (
            std::fs::canonicalize(path.as_str()),
            std::fs::canonicalize(tree.path().as_str()),
        );
        match real {
            (Ok(file), Ok(root)) if file.starts_with(&root) => Ok(Self(path)),
            _ => Err(ToolError::LeavesTree),
        }
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
pub fn check_path_dirs(env: &[(String, String)]) -> Result<(), ToolError> {
    for (_, value) in env.iter().filter(|(name, _)| name == "PATH") {
        for dir in value.split(':') {
            let path = AbsPath::parse(dir).map_err(ToolError::BadPath)?;
            let found = std::fs::symlink_metadata(path.as_str())
                .map_err(|e| ToolError::Unreadable(e.kind()))?;
            if found.file_type().is_symlink() {
                return Err(ToolError::PathEntrySymlink(dir.to_owned()));
            }
            if !found.is_dir() {
                return Err(ToolError::PathEntryNotDir(dir.to_owned()));
            }
        }
    }
    Ok(())
}
