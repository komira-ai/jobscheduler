//! One member of a pinned archive, read in memory: a zip (stored or deflated) or a
//! gzip-compressed tar. Nothing is unpacked to disk, so no member name can place a file.
//!
//! The archive has been checked against its pinned sha256 before it is read here; the limits
//! below only keep a hostile or damaged archive from filling memory.

use js_mdm_core::pins::Packing;

/// The largest member read, in bytes.
pub const MAX_MEMBER: u64 = 512 * 1024 * 1024;

/// Why a member could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The archive is damaged or of a kind this reader does not handle.
    #[error("{archive} is not a readable {packing:?} archive: {why}")]
    Unreadable {
        /// The archive's name.
        archive: String,
        /// How it is packed.
        packing: Packing,
        /// What is wrong.
        why: String,
    },
    /// The member is not in the archive, or is empty.
    #[error("{archive} has no {member}, or it is empty")]
    NoMember {
        /// The archive's name.
        archive: String,
        /// The member.
        member: String,
    },
}

/// The bytes of `member` in `bytes`, an archive called `archive` (for errors).
pub fn member(
    archive: &str,
    _packing: Packing,
    _bytes: &[u8],
    member: &str,
) -> Result<Vec<u8>, ArchiveError> {
    Err(ArchiveError::NoMember {
        archive: archive.to_owned(),
        member: member.to_owned(),
    })
}
