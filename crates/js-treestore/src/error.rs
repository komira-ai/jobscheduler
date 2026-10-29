//! The tree store's errors and its step names.

use crate::blobstore::{LabelError, StoreError};
use crate::sealer::SealError;

/// The steps of making a tree, in order. A crash hook is asked after each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// The layer blob is pinned.
    Pinned,
    /// The stage directory exists.
    Staged,
    /// The layer is extracted into the stage.
    Unpacked,
    /// The stage is sealed.
    Sealed,
    /// The stage is renamed to the tree's place.
    Renamed,
    /// The record is written.
    Recorded,
    /// The tree's bytes are registered with the blob store.
    Registered,
}

/// Why an operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The blob store refused.
    #[error("blob store: {0}")]
    Store(#[from] StoreError),
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
    /// The crash hook stopped the operation after the step, as a dying process would.
    #[error("stopped after {0:?}")]
    Crashed(Step),
}

pub(crate) fn ctx(what: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { what, source }
}
