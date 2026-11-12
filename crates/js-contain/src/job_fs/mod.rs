//! The mounts a root daemon makes on the host for a job's file system: each host path
//! opened beneath the directory it is allowed under, cloned into a private detached mount
//! of its own and attached at a fixed point; an overlay of read-only trees with a writable
//! upper layer, for a root of layers or for a copy-on-write mount; and their unmounts.
//! `docs/design/job-rootfs.md` (sections 4.2 to 4.4 and 4.6) is the design.
//!
//! 1. [`open_root`] opens a directory a job may name paths beneath, and [`open_beneath`]
//!    opens a path beneath it with `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`: a
//!    link that leads out of the directory fails then ([`ResolveError::Escapes`]), at the
//!    one moment that counts. A sub-path is opened the same way beneath what was opened.
//! 2. [`clone_at`] clones what was opened, with the mounts below it, into a detached mount
//!    (`open_tree(OPEN_TREE_CLONE | AT_RECURSIVE)`), sets it while it is still detached
//!    (`mount_setattr(AT_RECURSIVE)`: private, `nosuid`, `nodev`, and read-only or `noexec`
//!    as [`Attributes`] says, all the way down), then attaches it at a point of the daemon's
//!    (`move_mount`). From then on nothing reads the path's text again, so a path swapped
//!    for a link after step 1 changes nothing.
//! 3. [`mount_overlay`] mounts an [`Overlay`] of such clones (and of an image's tree) as
//!    the daemon: `nosuid`, `nodev`, and `noexec` when asked, with fixed options
//!    ([`OVERLAY_OPTIONS`]).
//! 4. [`unmount_all`] detaches the points it is given, deepest first; [`make_private_dir`]
//!    makes the directory that holds the points a private mount of its own, so that nothing
//!    mounted below it propagates anywhere.
//!
//! The system calls are [`MountCalls`], so that what is built from them is tested with a
//! fake as an ordinary user; [`Kernel`] makes them. They need Linux 5.12 (`mount_setattr`;
//! `openat2` needs 5.6, `open_tree` and `move_mount` 5.2) and, but for the resolver, root.
//! Which paths a job may name, where the points are and who owns the overlay's directories
//! are the caller's.

mod kernel;
mod resolve;

#[cfg(test)]
mod kernel_tests;
#[cfg(test)]
mod resolve_tests;
#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::fmt;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

pub use kernel::{Kernel, probe};
pub use resolve::{ResolveError, open_beneath, open_root};

/// The options every overlay this module mounts is given, besides its directories: no
/// index, no metacopy, no redirects (a directory that came from a lower cannot be renamed:
/// `EXDEV`), no inode number translation. The daemon mounts it as root in its own mount
/// namespace, so the upper layer's whiteouts are the kernel's character devices and
/// `userxattr` is not given.
pub const OVERLAY_OPTIONS: &str = "index=off,metacopy=off,redirect_dir=off,xino=off";

/// The most bytes of options `mount(2)` passes to a file system: one page, the last byte of
/// which the kernel overwrites with the string's end. A page is 4096 bytes or more.
pub const MAX_OVERLAY_OPTIONS: usize = 4095;

/// The bytes no path in an overlay's options may hold: `,` ends an option, `:` separates
/// two lowers and `\` escapes either.
const SEPARATORS: [char; 3] = [',', ':', '\\'];

/// The system calls this module is made of, one method each. [`Kernel`] makes them; a test
/// gives a fake.
pub trait MountCalls {
    /// `open_tree(source, "", OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_RECURSIVE |
    /// AT_EMPTY_PATH)`: a detached copy of the mount at `source` and of every mount below
    /// it, which the kernel removes when the descriptor is closed unless it was attached.
    ///
    /// # Errors
    /// The kernel's.
    fn clone_tree(&self, source: BorrowedFd<'_>) -> io::Result<OwnedFd>;

    /// `mount_setattr(tree, "", AT_EMPTY_PATH | AT_RECURSIVE, ...)` with what [`Attributes`]
    /// asks for, over `tree` and every mount below it.
    ///
    /// # Errors
    /// The kernel's.
    fn set_attributes(&self, tree: BorrowedFd<'_>, attributes: Attributes) -> io::Result<()>;

    /// `move_mount(tree, "", AT_FDCWD, point, MOVE_MOUNT_F_EMPTY_PATH)`: attaches the
    /// detached `tree` at the directory `point`.
    ///
    /// # Errors
    /// The kernel's.
    fn attach(&self, tree: BorrowedFd<'_>, point: &Path) -> io::Result<()>;

    /// `mount("overlay", point, "overlay", MS_NOSUID | MS_NODEV, options)`, with
    /// `MS_NOEXEC` too when `no_exec`.
    ///
    /// # Errors
    /// The kernel's.
    fn mount_overlay(&self, point: &Path, no_exec: bool, options: &str) -> io::Result<()>;

    /// `umount2(point, MNT_DETACH | UMOUNT_NOFOLLOW)`: the mount at `point` and every mount
    /// below it leave the tree at once, and go when no one uses them.
    ///
    /// # Errors
    /// The kernel's: `EINVAL` when `point` is not a mount point.
    fn detach(&self, point: &Path) -> io::Result<()>;

    /// Whether the directory `dir` is the root of a mount: its mount differs from its
    /// parent's (`statx`, `STATX_MNT_ID`). Never true of `/`.
    ///
    /// # Errors
    /// `statx` failed, or the kernel does not report mount ids (Linux 5.8).
    fn is_mount_point(&self, dir: &Path) -> io::Result<bool>;

    /// `mount(dir, dir, NULL, MS_BIND | MS_REC, NULL)`: `dir` becomes a mount of its own,
    /// with whatever is mounted below it.
    ///
    /// # Errors
    /// The kernel's.
    fn bind_onto_itself(&self, dir: &Path) -> io::Result<()>;

    /// `mount(NULL, dir, NULL, MS_PRIVATE | MS_REC, NULL)`: the mount at `dir`, and every
    /// mount below it, neither receives nor sends mount events.
    ///
    /// # Errors
    /// The kernel's.
    fn make_private(&self, dir: &Path) -> io::Result<()>;
}

/// How a clone is set while it is still detached, besides what every clone is: private
/// (no mount event reaches it or leaves it), `nosuid` and `nodev`, all the way down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    /// The clone, and every mount below it, cannot be written.
    pub read_only: bool,
    /// No file in the clone, nor below it, may be run or mapped executable.
    pub no_exec: bool,
}

/// An overlay to mount: read-only trees under a writable layer, which takes every write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlay {
    /// The read-only trees, top first (overlay's order: a file of an earlier one hides the
    /// same name in a later one). At least one.
    pub lowers: Vec<PathBuf>,
    /// The writable layer: an empty directory on a local file system. Its root's owner and
    /// mode are the overlay root's.
    pub upper: PathBuf,
    /// The overlay's work directory: an empty directory on the upper's file system.
    pub work: PathBuf,
    /// No file in the overlay may be run or mapped executable.
    pub no_exec: bool,
}

/// The system call a [`MountError`] stopped at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountStep {
    /// Cloning the opened tree ([`MountCalls::clone_tree`]).
    Clone,
    /// Setting the detached clone ([`MountCalls::set_attributes`]).
    Attributes,
    /// Attaching the clone ([`MountCalls::attach`]).
    Attach,
    /// Writing the overlay's options: a path that no option can carry, no lower, or too
    /// many bytes. No system call was made.
    Options,
    /// Mounting the overlay ([`MountCalls::mount_overlay`]).
    Overlay,
    /// Unmounting ([`MountCalls::detach`]).
    Unmount,
    /// Finding whether a directory is a mount point ([`MountCalls::is_mount_point`]).
    Inspect,
    /// Binding a directory onto itself ([`MountCalls::bind_onto_itself`]).
    Bind,
    /// Making a mount private ([`MountCalls::make_private`]).
    Private,
}

impl fmt::Display for MountStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clone => "clone the tree for",
            Self::Attributes => "set the clone's attributes for",
            Self::Attach => "attach the clone at",
            Self::Options => "write the overlay's options for",
            Self::Overlay => "mount the overlay at",
            Self::Unmount => "unmount",
            Self::Inspect => "find whether this is a mount point:",
            Self::Bind => "bind onto itself",
            Self::Private => "make private",
        })
    }
}

/// A mount, an unmount or a step towards one failed.
#[derive(Debug, thiserror::Error)]
#[error("{step} {}: {source}", point.display())]
pub struct MountError {
    /// The step that failed.
    pub step: MountStep,
    /// The mount point it was for.
    pub point: PathBuf,
    /// What the kernel answered, or (for [`MountStep::Options`]) why no call was made.
    #[source]
    pub source: io::Error,
}

/// The error of `step` at `point`, as a function of the kernel's answer.
fn failed(step: MountStep, point: &Path) -> impl FnOnce(io::Error) -> MountError + '_ {
    move |source| MountError {
        step,
        point: point.to_owned(),
        source,
    }
}

/// Clones the tree open as `source` (a directory [`open_beneath`] opened), with the mounts
/// below it, sets the clone as `attributes` says while it is detached, and attaches it at
/// the directory `point`. A clone that fails to be set or attached is closed, and so goes.
///
/// # Errors
/// The step that failed, with the kernel's answer.
pub fn clone_at(
    calls: &dyn MountCalls,
    source: BorrowedFd<'_>,
    point: &Path,
    attributes: Attributes,
) -> Result<(), MountError> {
    let tree = calls
        .clone_tree(source)
        .map_err(failed(MountStep::Clone, point))?;
    calls
        .set_attributes(tree.as_fd(), attributes)
        .map_err(failed(MountStep::Attributes, point))?;
    calls
        .attach(tree.as_fd(), point)
        .map_err(failed(MountStep::Attach, point))
}

/// Mounts `overlay` at the directory `point`.
///
/// # Errors
/// A path of the overlay cannot be carried by its options ([`MountStep::Options`]), or the
/// mount failed.
pub fn mount_overlay(
    calls: &dyn MountCalls,
    overlay: &Overlay,
    point: &Path,
) -> Result<(), MountError> {
    let options = overlay_options(overlay).map_err(|why| MountError {
        step: MountStep::Options,
        point: point.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, why),
    })?;
    calls
        .mount_overlay(point, overlay.no_exec, &options)
        .map_err(failed(MountStep::Overlay, point))
}

/// The options of `overlay`: `lowerdir=`, `upperdir=`, `workdir=`, then
/// [`OVERLAY_OPTIONS`].
///
/// # Errors
/// No lower, a path that is not UTF-8 or holds a byte of [`SEPARATORS`], or more than
/// [`MAX_OVERLAY_OPTIONS`] bytes.
fn overlay_options(overlay: &Overlay) -> Result<String, String> {
    if overlay.lowers.is_empty() {
        return Err("an overlay needs a lower".to_owned());
    }
    let text = |path: &Path| match path.to_str() {
        Some(text) if !text.contains(SEPARATORS) => Ok(text.to_owned()),
        _ => Err(format!(
            "{} cannot be given to an overlay: it is not UTF-8, or holds `,`, `:` or `\\`",
            path.display()
        )),
    };
    let lowers = overlay
        .lowers
        .iter()
        .map(|lower| text(lower))
        .collect::<Result<Vec<_>, _>>()?;
    let options = format!(
        "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
        lowers.join(":"),
        text(&overlay.upper)?,
        text(&overlay.work)?,
    );
    if options.len() > MAX_OVERLAY_OPTIONS {
        return Err(format!(
            "its options are {} bytes, more than the {MAX_OVERLAY_OPTIONS} a mount takes",
            options.len()
        ));
    }
    Ok(options)
}

/// Detaches every mount at `points`, deepest first, and of two at the same depth the later
/// in the list first (a point is listed before its mount is made, so the last made goes
/// first). A point that is not a mount point (`EINVAL`) or is not there (`ENOENT`) is
/// already unmounted: a point listed before a mount that was never made.
///
/// # Errors
/// The first point that could not be unmounted. The points after it are still tried.
pub fn unmount_all(calls: &dyn MountCalls, points: &[PathBuf]) -> Result<(), MountError> {
    let mut order: Vec<(usize, &PathBuf)> = points.iter().enumerate().collect();
    order.sort_by_key(|(at, point)| Reverse((point.components().count(), *at)));
    let mut first = None;
    for (_, point) in order {
        match calls.detach(point) {
            Err(why) if !not_mounted(&why) => {
                first.get_or_insert_with(|| failed(MountStep::Unmount, point)(why));
            }
            _ => {}
        }
    }
    first.map_or(Ok(()), Err)
}

/// Whether `umount2` answered that there was nothing to unmount.
fn not_mounted(why: &io::Error) -> bool {
    matches!(why.raw_os_error(), Some(libc::EINVAL | libc::ENOENT))
}

/// Makes the directory `dir` a mount point by binding it onto itself, unless it is one
/// already (a daemon that restarts finds its own bind, with the mounts it made below it,
/// and must not cover them with a second), then makes that mount and every mount below it
/// private, whether it was made or found.
///
/// # Errors
/// The step that failed, with the kernel's answer.
pub fn make_private_dir(calls: &dyn MountCalls, dir: &Path) -> Result<(), MountError> {
    let mounted = calls
        .is_mount_point(dir)
        .map_err(failed(MountStep::Inspect, dir))?;
    if !mounted {
        calls
            .bind_onto_itself(dir)
            .map_err(failed(MountStep::Bind, dir))?;
    }
    calls
        .make_private(dir)
        .map_err(failed(MountStep::Private, dir))
}
