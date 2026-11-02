//! Pins of the image store: a lease's use of an image, seen by every backend on the store.
//!
//! Backends (agents) share a scratch directory and its image store ([`super::lock`]), and
//! each one collects the store ([`super::gc`]) by its own lists: the digests it is
//! configured with and those of its own leases. The image of another backend's lease is on
//! neither list, and the per-digest gate of the store ([`super::image`]) lives in one
//! process. So each lease also *pins* its image: it holds `LOCK_SH` (`flock`) on the pin
//! file `<scratch>/oci-image-pins/<hex>` for as long as it holds the [`ImagePin`]. A
//! collection, in whichever backend, removes a digest only while it holds `LOCK_EX|LOCK_NB`
//! on the same file ([`Claim`]): a pin anywhere refuses it, and the digest is kept.
//!
//! **The race.** A pin and a removal decide under the one lock:
//! - A lease takes its pin first, then makes sure its image is there (fetching it if not),
//!   then runs. While the pin is held no collection gets the lock, so an image that is there
//!   once the pin is held stays there until the pin goes. Taking the pin waits while a
//!   collection holds the lock, which it holds only to set the digest aside (a rename).
//! - A collection that has the lock sets the digest aside (the image is gone at once), and
//!   removes the pin file before it lets the lock go.
//! - A lock taken on a pin file that was removed in between (the open came before the
//!   collection's unlink, the lock after) is a lock nobody else sees. So once a lock is
//!   taken, the name must still name the file that was locked; if it does not, the file is
//!   opened (made) again and locked again.
//!
//! A pin file of a digest that is still stored stays when its pins go: one empty file per
//! stored digest. A process's locks end with it, so a backend that died pins nothing. Each
//! pin file is opened as the scratch lock file is ([`super::lock::open_lock_file`]): not
//! through a link, and only if it is a regular file of the running user. The directory is
//! `0700`: only the backends' user reaches it.

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation as Op, flock};

use super::image::Images;
use super::lock::open_lock_file;

/// The directory of the pin files, beside the image store in a backend's scratch directory.
pub(crate) const PINS_DIR: &str = "oci-image-pins";

/// A lease's pin of one image ([`Oci::pin_image`](super::Oci::pin_image)): while it is held,
/// no backend on the store removes the image. Dropping it lets the image go.
///
/// The lock is let go with an explicit unlock, not by closing the file: a child forked by
/// another thread holds a copy of the descriptor until it execs.
#[must_use = "the image is pinned only while this is held"]
pub struct ImagePin {
    file: File,
}

impl Drop for ImagePin {
    fn drop(&mut self) {
        let _ = flock(&self.file, Op::Unlock);
    }
}

/// A collection's hold on the pin file of one digest: while it is held, no lease pins the
/// digest. Dropping it lets the lock go and leaves the file; [`Claim::retire`] removes the
/// file first.
pub(super) struct Claim {
    file: File,
    path: PathBuf,
}

impl Claim {
    /// For a digest that was set aside: removes its pin file, then lets the lock go. A lease
    /// that opened the file before the removal finds, once it has the lock, that the name no
    /// longer names it, and pins a new file.
    pub(super) fn retire(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = flock(&self.file, Op::Unlock);
    }
}

impl Images {
    /// Pins the digest whose hex is `hex` for a lease: [`ImagePin`]. Waits while a
    /// collection holds the digest's pin file.
    ///
    /// # Errors
    /// The pin directory or file could not be made or opened, or is not as required.
    pub(crate) fn pin(&self, hex: &str) -> std::io::Result<ImagePin> {
        self.pin_with(hex, &mut || {})
    }

    /// [`Images::pin`], calling `between` after each open of the pin file and before its
    /// lock: where a test puts another backend's step.
    pub(super) fn pin_with(
        &self,
        hex: &str,
        between: &mut dyn FnMut(),
    ) -> std::io::Result<ImagePin> {
        let file = lock(self.pins(), hex, Op::LockShared, between)?;
        Ok(ImagePin { file })
    }

    /// Claims the digest whose hex is `hex` for a removal: `None` when a lease of some
    /// backend pins it (or another collection has claimed it), without waiting.
    ///
    /// # Errors
    /// The pin directory or file could not be made or opened, or is not as required.
    pub(super) fn claim(
        &self,
        hex: &str,
        between: &mut dyn FnMut(),
    ) -> std::io::Result<Option<Claim>> {
        match lock(self.pins(), hex, Op::NonBlockingLockExclusive, between) {
            Ok(file) => Ok(Some(Claim {
                file,
                path: self.pins().join(hex),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Opens the pin file `<pins>/<hex>` (made, with its directory, if it is not there), calls
/// `between`, and locks it with `op`; again, until the file locked is the one the name
/// names once the lock is held.
///
/// # Errors
/// The directory or the file could not be made or opened, or is not as required; or the
/// lock was refused (`WouldBlock` for a lock that does not wait and is held by another).
fn lock(pins: &Path, hex: &str, op: Op, between: &mut dyn FnMut()) -> std::io::Result<File> {
    let path = pins.join(hex);
    loop {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(pins)?;
        let file = open_lock_file(&path)?;
        between();
        flock(&file, op)?;
        if names(&path, &file)? {
            return Ok(file);
        }
        // A collection removed the file between the open and the lock.
        let _ = flock(&file, Op::Unlock);
    }
}

/// Whether `path` names the file `file` is open on: the same device and inode, a link not
/// followed. A name that cannot be looked at names nothing (the next open says why).
fn names(path: &Path, file: &File) -> std::io::Result<bool> {
    let held = file.metadata()?;
    let named = std::fs::symlink_metadata(path);
    Ok(named.is_ok_and(|named| (named.dev(), named.ino()) == (held.dev(), held.ino())))
}
