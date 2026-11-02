//! Pins of the image store: a lease's use of an image, seen by every backend on the store and
//! kept until the lease is removed, whatever happens to the process that made it.
//!
//! Backends (agents) share a scratch directory and its image store ([`super::lock`]), and
//! each one collects the store ([`super::gc`]) by its own lists: the digests it is
//! configured with and those of its own leases. The image of another backend's lease is on
//! neither list, and the per-digest gate of the store ([`super::image`]) lives in one
//! process. So each lease also *pins* its image ([`Oci::pin_image`](super::Oci::pin_image)):
//! a file `<scratch>/oci-image-pins/leases/<lease>` that names the digest. It is a file, not
//! a lock a process holds, because a lease's container may outlive the process that made it
//! (an agent killed with its container still running): only the removal of the lease, by
//! its backend or by the sweep of the next start, removes the file
//! ([`Oci::unpin_image`](super::Oci::unpin_image)), once the container is gone.
//!
//! **The race.** A pin and a removal decide under one lock per digest, `flock` on
//! `<scratch>/oci-image-pins/<hex>`:
//! - A lease writes its pin while it holds `LOCK_SH` on the digest's lock file, then makes
//!   sure its image is there (fetching it if not), then runs.
//! - A removal (a collection, or the removal of one image) takes `LOCK_EX|LOCK_NB`
//!   ([`Claim`]). Refused (a pin is being written), or granted while a pin file names the
//!   digest, it keeps the digest. Granted with no pin, it sets the digest aside (the image is
//!   gone at once) and removes the lock file before it lets the lock go. So an image that is
//!   there once its lease's pin is written stays until the pin is removed; one that was set
//!   aside first is fetched again by the lease's check.
//! - A lock taken on a lock file that was removed in between (the open came before the
//!   removal's unlink, the lock after) is a lock nobody else sees. So once a lock is taken,
//!   the name must still name the file that was locked; if it does not, the file is opened
//!   (made) again and locked again.
//!
//! **Files.** The directory `oci-image-pins` and its `leases` are opened without following a
//! link and must be directories of the running user that others may not use (no group or
//! other permission): another user who could write there could remove a pin. Everything in
//! them is reached through their descriptors, never by a path again. A lock file must be a
//! regular file of the running user ([`super::lock`]'s rule for its own lock file). A digest
//! is 64 lowercase hex digits and a lease name is `[A-Za-z0-9._-]` not starting with `.`;
//! anything else is refused before anything is made. A lock file of a digest that is still
//! stored stays: one empty file per stored digest.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FlockOperation as Op, Mode, OFlags, flock};

use super::image::Images;
use super::lock::plain_file_of;

/// The directory of the pins, beside the image store in a backend's scratch directory.
pub(crate) const PINS_DIR: &str = "oci-image-pins";

/// The directory of the pin files of the leases, in [`PINS_DIR`].
pub(crate) const LEASES_DIR: &str = "leases";

/// A lock file, unlocked when this is dropped. The lock is let go with an explicit unlock,
/// not by closing the file: a child forked by another thread holds a copy of the descriptor
/// until it execs.
struct Held(File);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = flock(&self.0, Op::Unlock);
    }
}

/// A removal's hold on the lock file of one digest that no lease pins: while it is held, no
/// lease pins the digest. Dropping it lets the lock go and leaves the file;
/// [`Claim::retire`] removes the file first.
pub(super) struct Claim {
    _held: Held,
    dir: OwnedFd,
    hex: String,
}

impl Claim {
    /// For a digest that was set aside: removes its lock file, then lets the lock go. A lease
    /// that opened the file before the removal finds, once it has the lock, that the name no
    /// longer names it, and locks a new file. A file that cannot be removed stays, and is
    /// then the one every lease and removal locks.
    pub(super) fn retire(self) {
        let _ = rustix::fs::unlinkat(&self.dir, self.hex.as_str(), AtFlags::empty());
    }
}

impl Images {
    /// Pins the digest whose hex is `hex` for the lease `lease`: writes its pin file, under
    /// the digest's lock (waiting while a removal holds it). A pin the lease had is replaced.
    ///
    /// # Errors
    /// The digest or the lease name is not one, or a directory or file of the pins could not
    /// be made or opened, or is not as required.
    pub(crate) fn pin(&self, hex: &str, lease: &str) -> std::io::Result<()> {
        self.pin_with(hex, lease, &mut || {})
    }

    /// [`Images::pin`], calling `between` after each open of the lock file and before its
    /// lock: where a test puts another backend's step.
    pub(super) fn pin_with(
        &self,
        hex: &str,
        lease: &str,
        between: &mut dyn FnMut(),
    ) -> std::io::Result<()> {
        lease_name(lease)?;
        let dir = private_dir(CWD, self.pins())?;
        let _held = lock(&dir, hex, Op::LockShared, between)?;
        let leases = private_dir(&dir, Path::new(LEASES_DIR))?;
        let tmp = format!(".{lease}.tmp");
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW;
        let file = rustix::fs::openat(&leases, tmp.as_str(), flags | OFlags::CLOEXEC, owner())?;
        File::from(file).write_all(hex.as_bytes())?;
        Ok(rustix::fs::renameat(&leases, tmp.as_str(), &leases, lease)?)
    }

    /// Removes the pin of the lease `lease`; a lease with no pin has nothing to remove.
    ///
    /// # Errors
    /// The lease name is not one, or a directory of the pins or the file could not be
    /// opened or removed.
    pub(crate) fn unpin(&self, lease: &str) -> std::io::Result<()> {
        lease_name(lease)?;
        let dir = private_dir(CWD, self.pins())?;
        let leases = private_dir(&dir, Path::new(LEASES_DIR))?;
        match rustix::fs::unlinkat(&leases, lease, AtFlags::empty()) {
            Err(rustix::io::Errno::NOENT) => Ok(()),
            done => Ok(done?),
        }
    }

    /// Claims the digest whose hex is `hex` for a removal: `None` when a lease of some
    /// backend pins it or is pinning it (or another removal has claimed it), without
    /// waiting. `between` is called after each open of the lock file and before its lock.
    ///
    /// # Errors
    /// The digest is not one, or a directory or file of the pins could not be made or
    /// opened, or is not as required.
    pub(super) fn claim(
        &self,
        hex: &str,
        between: &mut dyn FnMut(),
    ) -> std::io::Result<Option<Claim>> {
        let dir = private_dir(CWD, self.pins())?;
        let held = match lock(&dir, hex, Op::NonBlockingLockExclusive, between) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            held => held?,
        };
        let leases = private_dir(&dir, Path::new(LEASES_DIR))?;
        if pinned(&leases, hex)? {
            return Ok(None);
        }
        Ok(Some(Claim {
            _held: held,
            dir,
            hex: hex.to_owned(),
        }))
    }
}

/// Mode `0600`, or `0700` for a directory: the running user's alone.
fn owner() -> Mode {
    Mode::RUSR | Mode::WUSR
}

/// Opens the directory `name` of `parent`, made `0700` if it is not there, without following
/// a link, and checks that it is the running user's and closed to others.
fn private_dir(parent: impl AsFd, name: &Path) -> std::io::Result<OwnedFd> {
    // One that is there already is checked below; one that could not be made fails the open.
    let _ = rustix::fs::mkdirat(&parent, name, Mode::RWXU);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let dir = rustix::fs::openat(&parent, name, flags, Mode::empty())?;
    private(&dir, rustix::process::geteuid().as_raw())?;
    Ok(dir)
}

/// Checks that the open directory `dir` belongs to `uid` and gives nothing to group or others.
pub(super) fn private(dir: &OwnedFd, uid: u32) -> std::io::Result<()> {
    let stat = rustix::fs::fstat(dir)?;
    let open = stat.st_mode & 0o077;
    if stat.st_uid == uid && open == 0 {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "not a directory of uid {uid} closed to others (owner {}, mode {:o})",
            stat.st_uid,
            stat.st_mode & 0o7777
        ),
    ))
}

/// Whether `hex` is the name of a digest: 64 lowercase hex digits.
pub(super) fn is_digest_name(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Refuses a lease name that is not `[A-Za-z0-9._-]`, not empty and not starting with `.`.
fn lease_name(name: &str) -> std::io::Result<()> {
    let plain = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain && !name.is_empty() && !name.starts_with('.') {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{name:?} is not a lease name"),
    ))
}

/// Whether a pin file in `leases` names the digest `hex`. A pin file that cannot be read
/// counts as naming it: the digest is kept.
fn pinned(leases: &OwnedFd, hex: &str) -> std::io::Result<bool> {
    for entry in Dir::read_from(leases)? {
        let entry = entry?;
        // `.`, `..` and a pin being written.
        if entry.file_name().to_bytes().starts_with(b".") {
            continue;
        }
        let read = read_at(leases, entry.file_name());
        if read.map_or(true, |text| text == hex) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The text of the file `name` of `dir`, a link not followed.
fn read_at(dir: &OwnedFd, name: &std::ffi::CStr) -> std::io::Result<String> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut text = String::new();
    File::from(rustix::fs::openat(dir, name, flags, Mode::empty())?).read_to_string(&mut text)?;
    Ok(text)
}

/// Opens the lock file of the digest `hex` in `dir` (made if it is not there), calls
/// `between`, and locks it with `op`; again, until the file locked is the one the name
/// names once the lock is held.
///
/// # Errors
/// The digest is not one, the file could not be made or opened or is not a regular file of
/// the running user, or the lock was refused (`WouldBlock` for a lock that does not wait
/// and is held by another).
fn lock(dir: &OwnedFd, hex: &str, op: Op, between: &mut dyn FnMut()) -> std::io::Result<Held> {
    if !is_digest_name(hex) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{hex:?} is not a sha256 digest"),
        ));
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    loop {
        let file = File::from(rustix::fs::openat(
            dir,
            hex,
            flags | OFlags::CLOEXEC,
            owner(),
        )?);
        plain_file_of(&file, rustix::process::geteuid().as_raw())?;
        between();
        flock(&file, op)?;
        let held = Held(file);
        if names(dir, hex, &held.0)? {
            return Ok(held);
        }
        // A removal removed the file between the open and the lock.
    }
}

/// Whether `hex` in `dir` names the file `file` is open on: the same device and inode, a
/// link not followed. A name that cannot be looked at names nothing (the next open says why).
fn names(dir: &OwnedFd, hex: &str, file: &File) -> std::io::Result<bool> {
    let held = file.metadata()?;
    let named = rustix::fs::statat(dir, hex, AtFlags::SYMLINK_NOFOLLOW);
    Ok(named.is_ok_and(|named| (named.st_dev, named.st_ino) == (held.dev(), held.ino())))
}
