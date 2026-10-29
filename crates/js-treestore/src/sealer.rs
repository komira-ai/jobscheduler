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

/// The platform's way to make a tree immutable. The macOS sealer (root, `uchg`) is a
/// separate component; the tests use a fake.
pub trait Sealer: Send + Sync + 'static {
    /// Re-checks the stage, normalises owner and modes, makes it immutable.
    ///
    /// # Errors
    /// The stage breaks a rule or a call fails.
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError>;
    /// Clears immutability on a tree, or a stage that was sealed in part, so it can be
    /// deleted. Called after the tree left its place.
    ///
    /// # Errors
    /// A call fails.
    fn unseal(&self, tree: &Path) -> Result<(), SealError>;
}
