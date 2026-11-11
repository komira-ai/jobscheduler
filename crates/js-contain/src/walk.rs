//! The descriptor-relative steps of a walk down a directory tree that never follows a
//! symlink and never recurses ([`crate::remove`] is the user): opening a directory,
//! naming it by device and inode, naming the mount it is on, and going back to its parent
//! only if the parent is the directory the walk came from.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, Statx, StatxFlags};

/// Opens a directory, refusing a symlink in its place.
pub const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A directory's device and inode.
pub type Identity = (u64, u64);

/// The identity of the directory open as `fd`.
///
/// # Errors
/// `fstat` failed.
pub fn identity(fd: &impl AsFd) -> std::io::Result<Identity> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((stat.st_dev, stat.st_ino))
}

/// The id of the mount `path` is on, read without following a link at its end, from the
/// directory `dir` (an empty `path` names `dir` itself). Two directories on one file system
/// have different mount ids when a bind of either is mounted between them, which their
/// device numbers do not show.
///
/// # Errors
/// `statx` failed, or the kernel does not report mount ids (`STATX_MNT_ID`, Linux 5.8).
pub(crate) fn mount_id_at(dir: impl AsFd, path: &Path) -> std::io::Result<u64> {
    let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::EMPTY_PATH;
    reported_mount_id(&rustix::fs::statx(dir, path, flags, StatxFlags::MNT_ID)?)
}

/// The mount id `stat` holds, if the kernel filled it in.
fn reported_mount_id(stat: &Statx) -> std::io::Result<u64> {
    if StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID) {
        Ok(stat.stx_mnt_id)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the kernel does not report mount ids (statx STATX_MNT_ID, Linux 5.8)",
        ))
    }
}

/// Opens the parent of `dir` through its `..`, refusing any directory but `expected`:
/// the walk goes back only where it came from.
///
/// # Errors
/// The open failed, or the parent is not `expected` (it was replaced).
pub fn back_to_parent(dir: &OwnedFd, expected: Identity) -> std::io::Result<OwnedFd> {
    let fd = rustix::fs::openat(dir, "..", DIRECTORY, Mode::empty())?;
    if identity(&fd)? != expected {
        return Err(std::io::Error::other(
            "the walk's parent directory was replaced",
        ));
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::CWD;

    /// Catches a way back that accepts a directory other than the one the walk came from
    /// (a parent swapped for another while the walk was below), and one that refuses the
    /// real parent.
    #[test]
    fn the_way_back_leads_only_to_the_parent_the_walk_came_from() {
        let dir = crate::unit_dir("walk-back");
        std::fs::create_dir(dir.join("child")).expect("mkdir");
        let top = rustix::fs::openat(CWD, &*dir, DIRECTORY, Mode::empty()).expect("open");
        let child = rustix::fs::openat(&top, "child", DIRECTORY, Mode::empty()).expect("open");
        let id = identity(&top).expect("identity");
        let back = back_to_parent(&child, id).expect("the same parent");
        assert_eq!(identity(&back).expect("identity"), id);
        let why = back_to_parent(&child, (id.0, id.1 ^ 1)).expect_err("another directory");
        assert!(why.to_string().contains("replaced"), "{why}");
    }

    /// Catches a mount id read from the wrong place, or one the kernel did not fill in being
    /// taken for an id: two directories of one mount have the same id, read by path or by
    /// descriptor (an empty path); a missing path fails; a `statx`
    /// whose mask lacks `STATX_MNT_ID` is refused as unsupported.
    #[test]
    fn a_mount_id_is_read_where_asked_and_only_when_reported() {
        let dir = crate::unit_dir("walk-mount-id");
        std::fs::create_dir(dir.join("child")).expect("mkdir");
        let top = rustix::fs::openat(CWD, &*dir, DIRECTORY, Mode::empty()).expect("open");
        let by_fd = mount_id_at(&top, Path::new("")).expect("by descriptor");
        assert_eq!(
            mount_id_at(CWD, &dir.join("child")).expect("by path"),
            by_fd
        );
        let missing = mount_id_at(&top, Path::new("missing")).expect_err("missing");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        let flags = AtFlags::EMPTY_PATH;
        let mut stat = rustix::fs::statx(&top, "", flags, StatxFlags::MNT_ID).expect("statx");
        assert_eq!(reported_mount_id(&stat).expect("reported"), by_fd);
        stat.stx_mask &= !StatxFlags::MNT_ID.bits();
        let why = reported_mount_id(&stat).expect_err("not reported");
        assert_eq!(why.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(
            why.to_string(),
            "the kernel does not report mount ids (statx STATX_MNT_ID, Linux 5.8)"
        );
    }

    /// Catches a directory open that follows a symlink or accepts a file.
    #[test]
    fn a_symlink_or_a_file_is_not_opened_as_a_directory() {
        let dir = crate::unit_dir("walk-open");
        std::fs::create_dir(dir.join("real")).expect("mkdir");
        std::os::unix::fs::symlink("real", dir.join("link")).expect("symlink");
        std::fs::write(dir.join("file"), b"f").expect("write");
        let top = rustix::fs::openat(CWD, &*dir, DIRECTORY, Mode::empty()).expect("open");
        assert!(rustix::fs::openat(&top, "real", DIRECTORY, Mode::empty()).is_ok());
        for name in ["link", "file"] {
            assert!(
                rustix::fs::openat(&top, name, DIRECTORY, Mode::empty()).is_err(),
                "{name} opened as a directory"
            );
        }
    }
}
