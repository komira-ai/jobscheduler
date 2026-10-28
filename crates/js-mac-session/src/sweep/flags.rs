//! Clearing the user flags and restoring the owner's mode on what a lease left in a
//! place the sweep does not remove (a home, a `/var/folders` entry): the walk of
//! [`clear_flags`] and [`clear_flags_within`], on the sweep's own primitives.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{Mode, OFlags, Stat};

use super::{
    PARK_DEPTH, StDev, Swept, examine, examine_inner, is_dir, is_kept, is_placed, listed, note,
    open_base, open_child, tolerate, vanished,
};

/// The user flags [`clear_flags`] clears: `uchg`, `uappnd`, `nodump`, `opaque` and
/// `hidden`, as the bits of `st_flags` (`UF_IMMUTABLE`, `UF_APPEND`, `UF_NODUMP`,
/// `UF_OPAQUE`, `UF_HIDDEN`), never spelled as `chflags` words: `nodump` there is the flag
/// itself, and its opposite is `dump`. The flags only root may change (`schg`,
/// `restricted`) are not in it: they are not the pool user's to set.
pub const CLEARED_FLAGS: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x8000;

/// Clears [`CLEARED_FLAGS`] on the directory `dir` itself, whoever owns it (the caller
/// names a home or a `/var/folders` entry, a place it knows), then, with the owner's
/// mode made `u+rwx`, on every entry below it that `uid` owns, at any depth: never through
/// a link, never past `dir`'s filesystem, and nothing of another owner's is changed (a
/// hard link to a system file keeps the flags and mode of its inode). The root first, so
/// a locked root does not stop the walk below it, and a clean's removal after this
/// finds nothing locked. Each error is recorded against its path and the walk goes on.
/// Flags are macOS's: elsewhere only the modes change.
#[must_use]
pub fn clear_flags(dir: &Path, uid: u32) -> Swept {
    let mut swept = Swept::default();
    let result = clear_base(dir, uid, &mut swept);
    note(&mut swept, dir, result);
    swept
}

/// [`clear_flags`] for a `/var/folders` entry, as the uid: `dir` itself and each directory
/// of it named in `keep` keep the flags, ACLs and mode the system gave them (it protects
/// those from the user and from root alike); everything below them, and every other entry
/// of `dir`, is cleared as [`clear_flags`] does. Each error is recorded against its path.
#[must_use]
pub fn clear_flags_within(dir: &Path, uid: u32, keep: &[&str]) -> Swept {
    let mut swept = Swept::default();
    let result = clear_within(dir, uid, keep, &mut swept);
    note(&mut swept, dir, result);
    swept
}

fn clear_within(path: &Path, uid: u32, keep: &[&str], swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(path)? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    for name in listed(&dir)? {
        let here = path.join(&name);
        let skipped = std::cell::Cell::new(0);
        let result =
            tolerate(examine(&dir, &name), (&dir, &name, &here), &skipped).and_then(|stat| {
                match stat {
                    Some(stat) if is_kept(&name, keep, &stat, dev) => {
                        open_child(&dir, &name, &stat)
                            .and_then(|child| clear_below(&child, &here, uid, dev, 1, swept))
                    }
                    Some(_) => tolerate(
                        clear_entry(&dir, &name, &here, (uid, dev, 0), swept),
                        (&dir, &name, &here),
                        &skipped,
                    ),
                    None => Ok(()),
                }
            });
        note(swept, &here, vanished(result));
    }
    Ok(())
}

/// [`clear_flags`] from the opened `path`.
fn clear_base(path: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let Some(dir) = open_base(path)? else {
        return Ok(());
    };
    clear_root(&dir)?;
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    clear_below(&dir, path, uid, dev, 0, swept)
}

/// Every entry of `dir`, cleared; errors are recorded, not returned (a listing that
/// fails is).
pub(super) fn clear_below(
    dir: &OwnedFd,
    path: &Path,
    uid: u32,
    dev: StDev,
    depth: usize,
    swept: &mut Swept,
) -> io::Result<()> {
    for name in listed(dir)? {
        let here = path.join(&name);
        let result = tolerate(
            clear_entry(dir, &name, &here, (uid, dev, depth), swept),
            (dir, &name, &here),
            &std::cell::Cell::new(0),
        );
        note(swept, &here, vanished(result));
    }
    Ok(())
}

/// One entry of [`clear_below`]'s directory: its flags and mode if `uid` owns it, then
/// what is below it if it is a directory on the same filesystem.
fn clear_entry(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    (uid, dev, depth): (u32, StDev, usize),
    swept: &mut Swept,
) -> io::Result<()> {
    let Some(stat) = examine_inner(dir, name)? else {
        return Ok(());
    };
    if stat.st_dev != dev {
        return Ok(());
    }
    // A directory the system placed keeps its flags and ACL; what is in it is cleared.
    if (stat.st_uid == uid) & !is_placed(dir, name, &stat) {
        clear_marked(dir, name, &stat)?;
        swept.removed += 1;
        make_owner_rwx(dir, name, &stat)?;
    }
    if !is_dir(&stat) {
        return Ok(());
    }
    if depth >= PARK_DEPTH {
        // Not searched deeper, so that no more than [`PARK_DEPTH`] descriptors are open
        // (a Mac's soft limit can be 256 in all): the removal that follows clears the
        // locks of what the uid owns as it goes, and a root removal needs nothing else.
        return Ok(());
    }
    let child = open_child(dir, name, &stat)?;
    clear_below(&child, here, uid, dev, depth + 1, swept)
}

/// Adds the owner's `rwx` to the entry `name` of `dir`, judged as `stat`, through a
/// descriptor opened without following a link and checked to be the entry judged: a link
/// swapped in since is never followed (`chmod` has no no-follow form on Linux), and one
/// that cannot be opened (a link, a socket, a file root alone may read) is left as it is,
/// which a removal by root does not need changed.
pub(super) fn make_owner_rwx(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    // Linux has no mode of a link to set (and refuses the flag for any entry).
    #[cfg(target_os = "macos")]
    let at = rustix::fs::AtFlags::SYMLINK_NOFOLLOW;
    #[cfg(not(target_os = "macos"))]
    let at = rustix::fs::AtFlags::empty();
    let (opened, file) = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(file) => (rustix::fs::fstat(&file)?, Some(file)),
        // A directory its owner took every permission from cannot be opened, by the
        // owner (a clean that runs as the uid): it is judged again by name, and its mode
        // set by name. (Never a link: opening one without following it answers ELOOP,
        // not EACCES, and the inode check below stands in front of the change.)
        Err(rustix::io::Errno::ACCESS) => (
            rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?,
            None,
        ),
        Err(_) => return Ok(()),
    };
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
        return Ok(());
    }
    match file {
        Some(file) => Ok(rustix::fs::fchmod(&file, owner_rwx(stat))?),
        None => Ok(rustix::fs::chmodat(dir, name, owner_rwx(stat), at)?),
    }
}

/// The permission bits of `stat` with the owner's `rwx` added.
#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
pub(super) fn owner_rwx(stat: &Stat) -> Mode {
    Mode::from_raw_mode(((stat.st_mode as u32 & 0o7777) | 0o700) as _)
}

/// Clears [`CLEARED_FLAGS`] of the directory `dir` itself.
#[cfg(target_os = "macos")]
pub(super) fn clear_root(dir: &OwnedFd) -> io::Result<()> {
    crate::macos::clear_dir_flags(dir, CLEARED_FLAGS)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
pub(super) fn clear_root(_dir: &OwnedFd) -> io::Result<()> {
    Ok(())
}

/// Clears [`CLEARED_FLAGS`] of `name` in `dir`, without following a link.
#[cfg(target_os = "macos")]
fn clear_marked(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    crate::macos::clear_flags_of(dir, name, stat.st_flags, CLEARED_FLAGS)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
fn clear_marked(_dir: &OwnedFd, _name: &OsStr, _stat: &Stat) -> io::Result<()> {
    Ok(())
}
