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
//! **Order at start.** Leases first, images second: the caller sweeps the leases a dead
//! daemon left (a separate step that this module does not do), passes the digests of the
//! leases that remain as `leased`, and only then reads the disk to offer.
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
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode};

use super::image::{ImageCheckError, Images, ROOTFS};
use super::{JobError, Oci};
use crate::walk::{DIRECTORY, Identity, back_to_parent, identity};

/// What one collection did.
#[derive(Clone, Debug, Default, PartialEq)]
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
        let failed = if self.failed.is_empty() {
            String::new()
        } else {
            format!(", could not remove: {}", self.failed.join("; "))
        };
        write!(
            f,
            "image store: removed {} digest(s) ({} bytes), kept {} ({} bytes in the store){failed}",
            self.removed.len(),
            self.removed_bytes,
            self.kept,
            self.kept_bytes
        )
    }
}

/// An `ensure` of a digest in progress: the digest is not collected while this lives.
pub(crate) struct InUse<'a> {
    images: &'a Images,
    hex: String,
}

impl Drop for InUse<'_> {
    fn drop(&mut self) {
        let mut book = self.images.book();
        let left = book.using.get_mut(&self.hex).map(|n| {
            *n = n.saturating_sub(1);
            *n
        });
        if left == Some(0) {
            book.using.remove(&self.hex);
        }
    }
}

impl Images {
    /// Marks `hex` in use until the result is dropped.
    ///
    /// # Errors
    /// The digest is being removed by a collection: the caller asks again.
    pub(crate) fn begin_use(&self, hex: &str) -> Result<InUse<'_>, ImageCheckError> {
        let mut book = self.book();
        if book.removing.contains(hex) {
            return Err(ImageCheckError::Failed(format!(
                "image sha256:{hex} is being removed from the store; ask again"
            )));
        }
        *book.using.entry(hex.to_owned()).or_default() += 1;
        Ok(InUse {
            images: self,
            hex: hex.to_owned(),
        })
    }

    /// Takes the digest `hex` for removal unless it is in use.
    fn claim(&self, hex: &str) -> bool {
        let mut book = self.book();
        if book.using.contains_key(hex) {
            return false;
        }
        book.removing.insert(hex.to_owned())
    }

    /// Ends the removal of `hex`; a removed digest forgets its single flight.
    fn release(&self, hex: &str, removed: bool) {
        let mut book = self.book();
        book.removing.remove(hex);
        if removed {
            book.flights.remove(hex);
        }
    }

    /// Removes every complete or partial digest directory not in `keep` (hex digests) and
    /// not in use, with `remove` (which gets the path and must not follow symlinks), and
    /// a digest directory that is not complete (no `rootfs`) whether or not it is named:
    /// an unpack renames a finished stage onto the digest, which cannot land on a leftover.
    ///
    /// # Errors
    /// The store directory cannot be read; a digest that cannot be removed is in the
    /// report instead.
    pub(crate) fn collect(
        &self,
        keep: &HashSet<String>,
        remove: &mut dyn FnMut(&Path) -> Result<(), String>,
    ) -> Result<GcReport, String> {
        let entries = match std::fs::read_dir(self.dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GcReport::default()),
            Err(e) => {
                return Err(format!(
                    "reading the image store {}: {e}",
                    self.dir().display()
                ));
            }
        };
        let mut hexes: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| is_digest_name(name))
            .collect();
        hexes.sort();
        let mut report = GcReport::default();
        for hex in hexes {
            let path = self.dir().join(&hex);
            let bytes = tree_bytes(&path).unwrap_or(0);
            if (keep.contains(&hex) && is_complete(&path)) || !self.claim(&hex) {
                report.kept += 1;
                report.kept_bytes += bytes;
                continue;
            }
            let result = remove(&path);
            self.release(&hex, result.is_ok());
            match result {
                Ok(()) => {
                    report.removed.push(format!("sha256:{hex}"));
                    report.removed_bytes += bytes;
                }
                Err(why) => {
                    report.failed.push(format!("sha256:{hex}: {why}"));
                    report.kept_bytes += bytes;
                }
            }
        }
        Ok(report)
    }
}

impl Oci {
    /// Removes the image digests the store holds that nothing names: not one of
    /// `configured` (the digests the executor is set up with) and not one of `leased`
    /// (the digest of every live lease). Digests are `sha256:<hex>` or the hex alone;
    /// anything else in a slice names nothing. Call it at start, before the disk
    /// offered is read; a lease running without its digest in `leased` loses its image.
    ///
    /// A digest being fetched or unpacked is kept; the second call after a collection
    /// removes nothing.
    ///
    /// # Errors
    /// The store directory cannot be read. A digest that could not be removed is in
    /// [`GcReport::failed`] and stays.
    pub fn collect_images(
        &self,
        configured: &[String],
        leased: &[String],
    ) -> Result<GcReport, String> {
        let keep: HashSet<String> = configured
            .iter()
            .chain(leased)
            .map(|d| d.strip_prefix("sha256:").unwrap_or(d).to_owned())
            .collect();
        let report = self
            .images
            .collect(&keep, &mut |path| self.remove_entry(path))?;
        tracing::info!("{report}");
        Ok(report)
    }

    /// Removes one digest directory, or unlinks a digest name that is not a directory.
    pub(super) fn remove_entry(&self, path: &Path) -> Result<(), String> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if !meta.is_dir() {
            return std::fs::remove_file(path).map_err(|e| e.to_string());
        }
        // The files are the container's ids' unless the daemon user may remove them: a
        // helper container removes what it cannot.
        crate::remove::remove_tree(path).or_else(|_| {
            self.remove_dir_blocking(path)
                .map_err(|e: JobError| e.to_string())
        })
    }
}

/// Whether `name` is the name of a digest directory: 64 lowercase hex digits.
fn is_digest_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `dir` is a directory (not a link) with a `rootfs` directory (not a link) in it.
fn is_complete(dir: &Path) -> bool {
    let is_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    is_dir(dir) && is_dir(&dir.join(ROOTFS))
}

/// A directory being measured.
struct Frame {
    id: Identity,
    subdirectories: Vec<OsString>,
}

/// The disk `dir` and everything below it holds, in 512-byte blocks, without recursion
/// and without following a symlink (the walk of [`crate::remove`], reading only). A
/// subdirectory the daemon user may not open counts for its own entry only.
fn tree_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut here = rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty())?;
    let mut total = blocks(&rustix::fs::fstat(&here)?);
    let mut stack = vec![Frame {
        id: identity(&here)?,
        subdirectories: measure_entries(&here, &mut total)?,
    }];
    while let Some(mut top) = stack.pop() {
        if let Some(name) = top.subdirectories.pop() {
            if let Ok(child) = rustix::fs::openat(&here, name.as_os_str(), DIRECTORY, Mode::empty())
            {
                let frame = Frame {
                    id: identity(&child)?,
                    subdirectories: measure_entries(&child, &mut total).unwrap_or_default(),
                };
                stack.extend([top, frame]);
                here = child;
            } else {
                stack.push(top);
            }
            continue;
        }
        if let Some(parent) = stack.last() {
            here = back_to_parent(&here, parent.id)?;
        }
    }
    Ok(total)
}

/// Adds the size of every entry of `dir` to `total` and returns the names of the
/// subdirectories.
fn measure_entries(dir: &rustix::fd::OwnedFd, total: &mut u64) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name));
        }
    }
    // An entry that vanished meanwhile is not counted.
    let stats = names.into_iter().filter_map(|name| {
        rustix::fs::statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|stat| (name, stat))
    });
    let mut subdirectories = Vec::new();
    for (name, stat) in stats {
        *total += blocks(&stat);
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            subdirectories.push(name);
        }
    }
    Ok(subdirectories)
}

/// The disk a file holds: its 512-byte blocks.
fn blocks(stat: &rustix::fs::Stat) -> u64 {
    u64::try_from(stat.st_blocks)
        .unwrap_or(0)
        .saturating_mul(512)
}
