//! Sealing: making an extracted stage a read-only tree, and undoing it for deletion.

use std::path::Path;

/// What sealing a stage found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sealed {
    /// The bytes the tree takes on the volume (`st_blocks` times the block size).
    pub bytes: u64,
}

/// The sealer failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SealError(pub String);

impl From<std::io::Error> for SealError {
    fn from(error: std::io::Error) -> SealError {
        SealError(error.to_string())
    }
}

impl From<rustix::io::Errno> for SealError {
    fn from(error: rustix::io::Errno) -> SealError {
        SealError(std::io::Error::from(error).to_string())
    }
}

/// The platform's way to make a tree immutable. The macOS sealer (root, `uchg`) is a
/// separate component; the tests use a fake.
///
/// Implementations must not panic.
pub trait Sealer: Send + Sync + 'static {
    /// Re-checks the stage, normalises owner and modes, makes it immutable.
    ///
    /// **Durability:** it returns only after the tree's file data and its directories have
    /// been synced to disk (`fsync` of every file and directory, or the platform's
    /// equivalent). The store writes the tree's record after this and after syncing
    /// `trees/` for the rename, so a record that survives a power loss never sits beside a
    /// tree with missing data. `unpack_layer` does not sync.
    ///
    /// # Errors
    /// The stage breaks a rule or a call fails.
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError>;
    /// Clears immutability on a tree, or on a stage that was sealed in part, and on
    /// everything under it, so that it can be deleted. Called on a tree after it was moved to
    /// `trash/`, and before a second try to move it when the first move was refused (the move
    /// of an immutable directory fails on some systems). It must be safe to call again, and
    /// on a tree that is not sealed. If the second move is refused too, the store calls
    /// [`Sealer::seal`] on the tree where it stands, so `seal` must accept a tree in place.
    ///
    /// # Errors
    /// A call fails.
    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError>;
}
