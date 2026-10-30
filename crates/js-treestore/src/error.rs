//! The tree store's errors and its step names.

use crate::sealer::SealError;
use crate::source::{FetchError, LabelError};

/// The steps of making a tree, in order. A crash hook is asked after each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// The blob is fetched and verified in `tmp/`.
    Fetched,
    /// The stage directory exists.
    Staged,
    /// The layer is extracted into the stage.
    Unpacked,
    /// The stage is sealed.
    Sealed,
    /// The stage is renamed to the tree's place.
    Renamed,
    /// The record is written; the tree is present from here on.
    Recorded,
}

/// Why an operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The source failed.
    #[error("fetch: {0}")]
    Fetch(#[from] FetchError),
    /// The fetched bytes are not the blob that was asked for.
    #[error("blob {digest} arrived as {got_size} bytes hashing to {got}")]
    Blob {
        /// The digest asked for.
        digest: String,
        /// The digest of what arrived.
        got: String,
        /// The size of what arrived.
        got_size: u64,
    },
    /// A name is not a valid label.
    #[error(transparent)]
    Label(#[from] LabelError),
    /// The layer was refused or could not be extracted.
    #[error("layer: {0}")]
    Unpack(#[from] js_bundle::UnpackError),
    /// Sealing failed.
    #[error("seal: {0}")]
    Seal(#[from] SealError),
    /// A file-system call failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The error.
        source: std::io::Error,
    },
    /// The tree is not present (or was made by an older extractor).
    #[error("tree {0} is not present")]
    Absent(String),
    /// The tree is pinned and cannot be replaced.
    #[error("tree is pinned")]
    Busy,
    /// The run already pins the tree.
    #[error("run already pins the tree")]
    AlreadyPinned,
    /// The budget or the free-space low-water cannot be met: everything left is pinned,
    /// kept or too recently used. Retriable.
    #[error("no space: {needed} bytes needed, {used} used of {budget}")]
    NoSpace {
        /// The bytes of the tree being added.
        needed: u64,
        /// The bytes trees use now.
        used: u64,
        /// The budget.
        budget: u64,
    },
    /// Another store holds the directory.
    #[error("the directory {0} is held by another tree store")]
    Locked(String),
    /// A tree is in place that this store does not hold as current, so it cannot be taken
    /// for the new content.
    #[error("tree {0} is in place but is not one this store holds")]
    Conflict(String),
    /// The crash hook stopped the operation after the step, as a dying process would.
    #[error("stopped after {0:?}")]
    Crashed(Step),
}

pub(crate) fn ctx(what: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { what, source }
}
