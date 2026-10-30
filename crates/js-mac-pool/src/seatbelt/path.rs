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
///
/// Canonicalisation is the caller's duty: symbolic links other than the three named
/// above, and Unicode normalisation of the spelling, are not checked here. The host step
/// that builds a [`SlotLayout`](super::SlotLayout) resolves every path through a
/// descriptor first and pins the result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsPath(String);

impl AbsPath {
    /// Checks `path`.
    ///
    /// # Errors
    /// A [`PathError`] naming the first rule the path breaks.
    pub fn new(path: &Path) -> Result<Self, PathError> {
        Self::parse(path.to_str().ok_or(PathError::NotUtf8)?)
    }

    /// Checks `text` as a path.
    ///
    /// # Errors
    /// A [`PathError`] naming the first rule the text breaks.
    pub fn parse(text: &str) -> Result<Self, PathError> {
        if text.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        let rest = text.strip_prefix('/').ok_or(PathError::NotAbsolute)?;
        if text.chars().any(char::is_control) {
            return Err(PathError::Control);
        }
        if rest.is_empty() {
            return Err(PathError::Root);
        }
        for component in rest.split('/') {
            if matches!(component, "" | "." | "..") {
                return Err(PathError::NotNormal);
            }
        }
        let first = rest.split('/').next().unwrap_or_default();
        if LINKED_ROOTS.contains(&first) {
            return Err(PathError::Linked(first.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This path with one more component, `name`, which is checked like the others.
    ///
    /// # Errors
    /// [`PathError::NotNormal`] for a `name` that is empty, `.`, `..` or holds a `/`; any
    /// other [`PathError`] the longer path breaks.
    pub fn child(&self, name: &str) -> Result<Self, PathError> {
        if name.contains('/') {
            return Err(PathError::NotNormal);
        }
        Self::parse(&format!("{}/{name}", self.0))
    }

    /// Whether this path is `other` or below it, by whole components (`/a/bc` is not
    /// under `/a/b`).
    #[must_use]
    pub fn starts_with(&self, other: &Self) -> bool {
        self.0
            .strip_prefix(other.0.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

impl AsRef<str> for AbsPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
