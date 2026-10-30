//! Removing a lease's scratch directory: without recursion and without following a
//! symlink.
//!
//! The job decides what its scratch directory holds, outputs or not, and how deep.
//! `std::fs::remove_dir_all` recurses once per directory level, so a tree a few
//! thousand levels deep that the job left anywhere in its upper directory ran the
//! clean step off its thread's stack and took the host process down. This walk is a loop
//! over a `Vec` of frames on the heap, as a collector of outputs would walk it:
//! only the directory being emptied holds a descriptor, every directory is opened
//! relative to its parent with `O_NOFOLLOW`, and the walk returns to a parent through
//! the child's `..`, refusing one that is not the directory it came from (device and
//! inode).
//!
//! Every entry is unlinked without being looked at first: `unlinkat` removes a file, a
//! symlink (never its target), a socket or a device, and refuses a directory with
//! `EISDIR`, which the walk then empties and removes.
//!
//! An entry that is gone by the time the walk reaches it (`ENOENT` from the unlink, the
//! open of a listed subdirectory or the rmdir) was removed by someone else: it is
//! already removed, and the walk goes on. The root itself is not such an entry.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, Mode};
use rustix::io::Errno;

use crate::walk::{DIRECTORY, Identity, back_to_parent, identity};

/// A directory being emptied.
struct Frame {
    /// Its name in its parent (unused for the top directory).
    name: OsString,
    id: Identity,
    /// The subdirectories still to empty and remove.
    subdirectories: Vec<OsString>,
}

/// What the walk is about to do to the entry `name` of the directory `dir`; see
/// [`remove_tree_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Unlink a listed entry.
    Unlink,
    /// Open a listed subdirectory to empty it.
    Open,
    /// Remove an emptied subdirectory from its parent.
    Rmdir,
}

/// Removes the directory `dir` and everything in it, at any depth. An error (a
/// directory the daemon's user may not read or write) stops the walk and leaves the
/// rest; `NotFound` means `dir` is already gone.
///
/// An entry that vanishes once the walk is underway (listed, then removed by someone
/// else before the walk reaches it) is already removed, not a failure. An entry that
/// appears (`ENOTEMPTY`) is one. An entry another process renames out of the tree
/// during the walk (same filesystem only) is gone from it too, so the walk succeeds
/// while that entry lives on where it was moved: the tree is removed, not every byte
/// that was once in it.
pub fn remove_tree(dir: &Path) -> std::io::Result<()> {
    remove_tree_with(dir, &mut |_, _, _| {})
}

/// [`remove_tree`], calling `before(step, directory, name)` just before each step of
/// the walk so a test can change the tree at that exact point.
fn remove_tree_with(
    dir: &Path,
    before: &mut dyn FnMut(Step, &OwnedFd, &OsStr),
) -> std::io::Result<()> {
    let mut here = rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty())?;
    let mut stack = vec![Frame {
        name: OsString::new(),
        id: identity(&here)?,
        subdirectories: unlink_all_but_directories(&here, before)?,
    }];
    while let Some(mut top) = stack.pop() {
        if let Some(name) = top.subdirectories.pop() {
            before(Step::Open, &here, &name);
            let child = match rustix::fs::openat(&here, name.as_os_str(), DIRECTORY, Mode::empty())
            {
                // Removed by someone else since the listing: carry on with the rest.
                Err(Errno::NOENT) => {
                    stack.push(top);
                    continue;
                }
                other => other?,
            };
            let frame = Frame {
                name,
                id: identity(&child)?,
                subdirectories: unlink_all_but_directories(&child, before)?,
            };
            stack.extend([top, frame]);
            here = child;
            continue;
        }
        // `here` is empty: remove it from its parent, the next frame down.
        if let Some(parent) = stack.last() {
            here = back_to_parent(&here, parent.id)?;
            before(Step::Rmdir, &here, &top.name);
            // Removed by someone else, which is what was wanted.
            gone_is_removed(rustix::fs::unlinkat(
                &here,
                top.name.as_os_str(),
                AtFlags::REMOVEDIR,
            ))?;
        }
    }
    drop(here);
    std::fs::remove_dir(dir)
}

/// A removal that found nothing to remove has done its job.
fn gone_is_removed(removal: Result<(), Errno>) -> Result<(), Errno> {
    match removal {
        Err(Errno::NOENT) => Ok(()),
        other => other,
    }
}

/// Unlinks every entry of `dir` that is not a directory, and returns the names of
/// those that are. The names are read first, then unlinked: no entry is removed while
/// the directory stream is being read.
fn unlink_all_but_directories(
    dir: &OwnedFd,
    before: &mut dyn FnMut(Step, &OwnedFd, &OsStr),
) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name));
        }
    }
    let mut subdirectories = Vec::new();
    for name in names {
        before(Step::Unlink, dir, &name);
        // An entry removed by someone else since the listing needs no unlink.
        match gone_is_removed(rustix::fs::unlinkat(
            dir,
            name.as_os_str(),
            AtFlags::empty(),
        )) {
            Err(Errno::ISDIR) => subdirectories.push(name),
            other => other?,
        }
    }
    Ok(subdirectories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh scratch directory for one unit test, beside the test binary.
    fn scratch(name: &str) -> crate::test_dir::TestDir {
        crate::unit_dir(&format!("remove-{name}"))
    }

    /// Makes `levels` directories nested in `dir` (`d/d/.../d`), by descriptor (the
    /// deepest path is longer than `PATH_MAX`), with a file, a symlink and a FIFO at
    /// the bottom.
    fn deep(dir: &Path, levels: usize) {
        let flags = DIRECTORY;
        let mut here = rustix::fs::openat(CWD, dir, flags, Mode::empty()).expect("open");
        for _ in 0..levels {
            rustix::fs::mkdirat(&here, "d", Mode::from_raw_mode(0o755)).expect("mkdir");
            here = rustix::fs::openat(&here, "d", flags, Mode::empty()).expect("open");
        }
        rustix::fs::symlinkat("/", &here, "link").expect("symlink");
        let fifo = rustix::fs::FileType::Fifo;
        rustix::fs::mknodat(&here, "fifo", fifo, Mode::RUSR, 0).expect("mkfifo");
        let file = rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE;
        rustix::fs::openat(&here, "f", file, Mode::RUSR).expect("create");
    }

    /// Catches a recursive removal: 20,000 levels on a 256 KiB stack, an eighth of a
    /// tokio blocking thread's, where `std::fs::remove_dir_all` overflows (the test
    /// binary aborts). Also catches a removal that leaves anything behind, at the bottom
    /// or beside the tree, and one that follows a symlink (`link` names `/`).
    #[test]
    fn a_tree_of_any_depth_is_removed_without_recursion() {
        let dir = scratch("remove-deep");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("wide/a")).expect("mkdir");
        std::fs::write(top.join("wide/b"), b"b").expect("write");
        deep(&top, 20_000);
        let removed = top.clone();
        std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || remove_tree(&removed))
            .expect("spawn")
            .join()
            .expect("the removal finished")
            .expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
        assert!(Path::new("/").exists());
        let gone = remove_tree(&top).expect_err("already gone");
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
    }

    /// Catches an entry the daemon's user cannot remove (its directory is not
    /// writable) being skipped, or reported as success: the removal fails, so the
    /// clean step falls back to a helper container.
    #[test]
    fn an_entry_that_cannot_be_unlinked_fails_the_removal() {
        let dir = scratch("remove-readonly");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("ro")).expect("mkdir");
        std::fs::write(top.join("ro/f"), b"f").expect("write");
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(top.join("ro"), mode(0o500)).expect("chmod");
        let why = remove_tree(&top).expect_err("ro/f stays");
        std::fs::set_permissions(top.join("ro"), mode(0o755)).expect("chmod back");
        assert_eq!(why.kind(), std::io::ErrorKind::PermissionDenied, "{why}");
        remove_tree(&top).expect("removed once writable");
    }

    /// Removes `name` from the directory open as `fd`, as another process would: a file
    /// or symlink, or (`dir`) an empty directory.
    fn take(fd: &OwnedFd, name: &OsStr, dir: bool) {
        let flags = if dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(fd, name, flags).expect("the entry is there to take");
    }

    /// Catches a listed file that is removed by someone else before the walk unlinks it
    /// (`ENOENT`) failing the removal, and a vanished entry that stops the walk short of
    /// the entries after it.
    #[test]
    fn a_file_removed_after_the_listing_is_already_removed() {
        let dir = scratch("vanish-file");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("sub")).expect("mkdir");
        for name in ["gone", "kept", "sub/gone", "sub/kept"] {
            std::fs::write(top.join(name), b"f").expect("write");
        }
        let mut before = |step, fd: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Unlink, OsStr::new("gone")) {
                take(fd, name, false);
            }
        };
        remove_tree_with(&top, &mut before).expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
    }

    /// Catches a listed subdirectory that vanishes before the walk opens it failing the
    /// removal.
    #[test]
    fn a_directory_removed_before_it_is_opened_is_already_removed() {
        let dir = scratch("vanish-open");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("gone")).expect("mkdir");
        std::fs::create_dir_all(top.join("kept/deeper")).expect("mkdir");
        let mut before = |step, fd: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Open, OsStr::new("gone")) {
                take(fd, name, true);
            }
        };
        remove_tree_with(&top, &mut before).expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
    }

    /// Catches an emptied subdirectory that someone else removes before the walk does
    /// (`ENOENT` from the rmdir) failing the removal.
    #[test]
    fn a_directory_removed_before_its_rmdir_is_already_removed() {
        let dir = scratch("vanish-rmdir");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("gone")).expect("mkdir");
        std::fs::create_dir_all(top.join("kept")).expect("mkdir");
        let mut before = |step, fd: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Rmdir, OsStr::new("gone")) {
                take(fd, name, true);
            }
        };
        remove_tree_with(&top, &mut before).expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
    }

    /// Catches a walk below a directory that is removed from under it (its file and
    /// then the directory itself, by someone else) failing, in the unlink or in the way
    /// back up to the parent.
    #[test]
    fn a_directory_removed_while_the_walk_is_in_it_is_already_removed() {
        let dir = scratch("vanish-below");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("sub")).expect("mkdir");
        std::fs::write(top.join("sub/f"), b"f").expect("write");
        let sub = top.join("sub");
        let mut before = |step, fd: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Unlink, OsStr::new("f")) {
                take(fd, name, false);
                std::fs::remove_dir(&sub).expect("remove sub");
            }
        };
        remove_tree_with(&top, &mut before).expect("removed");
        assert!(std::fs::symlink_metadata(&top).is_err(), "the tree is left");
    }

    /// Catches an entry added during the walk (the directory is no longer empty,
    /// `ENOTEMPTY`) being reported as removed: it is a failure, and the new file stays.
    #[test]
    fn an_entry_added_during_the_walk_fails_the_removal() {
        let dir = scratch("added");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("sub")).expect("mkdir");
        let late = top.join("sub/late");
        let mut before = |step, _: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Rmdir, OsStr::new("sub")) {
                std::fs::write(&late, b"f").expect("write");
            }
        };
        let why = remove_tree_with(&top, &mut before).expect_err("sub is not empty");
        assert_eq!(why.kind(), std::io::ErrorKind::DirectoryNotEmpty, "{why}");
        assert!(late.exists());
    }

    /// Catches the top-level path being treated as vanished: a root that is not there is
    /// still `NotFound`, which a caller can tell from a removal.
    #[test]
    fn a_missing_root_is_still_not_found() {
        let dir = scratch("missing-root");
        let why = remove_tree(&dir.join("never")).expect_err("no such directory");
        assert_eq!(why.kind(), std::io::ErrorKind::NotFound);
    }

    /// Catches a walk that follows a symlink: one in a listing is unlinked and its target
    /// kept, and a subdirectory swapped for a symlink after the listing is not opened
    /// (the removal fails, the target and its file stay).
    #[test]
    fn a_symlink_is_never_followed() {
        let dir = scratch("symlink");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("f"), b"f").expect("write");
        let top = dir.join("top");
        std::fs::create_dir_all(top.join("swapped")).expect("mkdir");
        std::os::unix::fs::symlink(&outside, top.join("link")).expect("symlink");
        let target = outside.clone();
        let mut before = |step, fd: &OwnedFd, name: &OsStr| {
            if (step, name) == (Step::Open, OsStr::new("swapped")) {
                take(fd, name, true);
                rustix::fs::symlinkat(&target, fd, name).expect("symlink");
            }
        };
        remove_tree_with(&top, &mut before).expect_err("a symlink is not a directory");
        assert!(outside.join("f").exists(), "the target was followed");
        assert!(top.join("swapped").is_symlink());
        assert!(!top.join("link").exists(), "the listed symlink is unlinked");
    }
}
