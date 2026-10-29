#![allow(unused)]
//! The one type a path takes on its way into a profile.

use std::path::Path;

/// The longest path a profile parameter takes, in bytes (macOS `PATH_MAX`).
pub const MAX_PATH_BYTES: usize = 1024;

/// The first components that are symbolic links on macOS; a grant spelled through one
/// names the link, not the directory behind it (`/var` is `/private/var`).
const LINKED_ROOTS: [&str; 3] = ["var", "tmp", "etc"];

/// Why a path is not accepted as a profile parameter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path is not valid UTF-8.
    #[error("path is not valid UTF-8")]
    NotUtf8,
    /// The path is longer than [`MAX_PATH_BYTES`].
    #[error("path is too long")]
    TooLong,
    /// The path does not start with `/`.
    #[error("path is not absolute")]
    NotAbsolute,
    /// The path contains a control character (NUL, newline, tab, escape, DEL and the
    /// other Unicode control characters).
    #[error("path contains a control character")]
    Control,
    /// The path is `/`: a grant of the whole file system.
    #[error("path is the root directory")]
    Root,
    /// The path has an empty, `.` or `..` component, or ends in `/`.
    #[error("path is not in normal form")]
    NotNormal,
    /// The path starts with a component that is a link; the canonical spelling is under
    /// `/private`.
    #[error("path starts with /{0}, a symbolic link; use the canonical /private spelling")]
    Linked(String),
}

/// An absolute path in normal form that is safe to hand to `sandbox-exec` as a `-D`
/// parameter value.
///
/// The check is on the spelling only: the caller canonicalises through a descriptor
/// first (a profile compares the path the kernel resolves, and `/var` resolves to
/// `/private/var`), and the check refuses the common links by name. Quotes, parentheses
/// and `=` are accepted because a parameter value never enters the profile text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsPath(String);

impl AbsPath {
    /// Checks `path`.
    ///
    /// # Errors
    /// A [`PathError`] naming the first rule the path breaks.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let text = path.as_ref().to_str().ok_or(PathError::NotUtf8)?;
        text.strip_prefix('/').ok_or(PathError::NotAbsolute)?;
        Ok(Self(text.to_owned()))
    }

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
