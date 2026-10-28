//! The descriptor-relative steps of a walk down a directory tree that never follows a
//! symlink and never recurses ([`crate::remove`] is the user): opening a directory,
//! naming it by device and inode, and going back to its parent only if the parent is the
//! directory the walk came from.

use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{Mode, OFlags};

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
