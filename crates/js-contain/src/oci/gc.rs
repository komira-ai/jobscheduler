//! Collecting the per-node image store: removing the digests nothing names.
//!
//! The store (`image`) holds one directory `<hex>/rootfs` per unpacked digest and, while
//! an unpack is under way, `.stage-*` directories beside them. This is the minimal
//! collection: at a start, with the digests the daemon is given as inputs, every complete
//! digest directory that is not named goes; there is no age or size policy and no
//! least-recently-used order (that comes with the in-use pin of a running lease, later).
//!
//! **Referenced** is what the caller passes and nothing this crate can see: the digests
//! the executor is configured with (its base and tool images, which a pull request
//! carries pinned) and the digest of every lease that is live. The agent has no such
//! list yet, so [`Oci::collect_images`] takes two explicit slices and the caller must put
//! every live lease's digest in the second; a lease whose digest is not listed loses its
//! root filesystem.
//!
//! **Safety against a pull in flight.** Every `ensure` of a digest holds an in-use mark
//! for its whole length ([`Images::begin_use`]); a digest with a mark is kept. A digest
//! being removed is marked too, and an `ensure` that arrives meanwhile is refused with
//! words that say to ask again, so it never sees an image half gone. The stage and blobs
//! directories of an unpack are not digest names and are never touched here (the sweep
//! at start removes the ones a dead daemon left). A removed digest also forgets its
//! single flight, or a later `ensure` would take the old success for the image.
//!
//! **Safety against a hostile tree.** An image's files are the container's, not the
//! daemon user's: a directory is removed with [`crate::remove::remove_tree`] (no
//! recursion, `O_NOFOLLOW` at every level, no symlink followed) and, if the daemon user
//! may not, by a helper container ([`Oci::remove_dir_blocking`]). A digest name that is
//! a symlink or a file is unlinked and never opened. Only names that are 64 lowercase hex
//! digits are looked at; anything else in the store is left alone.
//!
//! **Disk accounting.** The store lives in the scratch volume, so the free space the
//! agent reads there already excludes it: it is not subtracted again from the disk the
//! node offers. What the store may *grow* by after that read (a new digest) is covered by
//! the low-water mark, which the operator sets at least as high as the unpacked size of
//! the largest image pinned. The collection runs before that read so that what it frees
//! is offered. [`GcReport`] says how many bytes went and how many stay.

use std::collections::HashSet;
use std::path::Path;

use super::image::{ImageCheckError, Images};
use super::Oci;

/// What one collection did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// The digests removed (`sha256:<hex>`), in order.
    pub removed: Vec<String>,
    /// The disk they held (512-byte blocks, as `du` counts), as far as the daemon user
    /// could read it: a lower bound when the image has directories only the container's
    /// ids may open.
    pub removed_bytes: u64,
    /// The digests kept: named, or in use.
    pub kept: usize,
    /// The disk the store still holds in digest directories (a removal that failed
    /// included), measured the same way.
    pub kept_bytes: u64,
    /// The digests that could not be removed, each with the reason.
    pub failed: Vec<String>,
}

impl std::fmt::Display for GcReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "image store: removed {} digest(s) ({} bytes), kept {} ({} bytes in the store)",
            self.removed.len(),
            self.removed_bytes,
            self.kept,
            self.kept_bytes
        )?;
        if !self.failed.is_empty() {
            write!(f, ", could not remove: {}", self.failed.join("; "))?;
        }
        Ok(())
    }
}

/// A stub: nothing is marked yet.
#[derive(Debug)]
pub(crate) struct InUse;

impl Images {
    /// A stub.
    pub(crate) fn begin_use(&self, _hex: &str) -> Result<InUse, ImageCheckError> {
        Ok(InUse)
    }

    /// A stub: removes nothing.
    pub(crate) fn collect(
        &self,
        _keep: &HashSet<String>,
        _remove: &mut dyn FnMut(&Path) -> Result<(), String>,
    ) -> Result<GcReport, String> {
        Ok(GcReport::default())
    }
}

impl Oci {
    /// A stub: removes nothing.
    ///
    /// # Errors
    /// Never yet.
    pub fn collect_images(
        &self,
        _configured: &[String],
        _leased: &[String],
    ) -> Result<GcReport, String> {
        Ok(GcReport::default())
    }
}
