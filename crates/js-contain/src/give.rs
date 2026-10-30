//! The give mode of the daemon's binary: gives a lease's input root's directories, and
//! only its directories, to an owner, run inside a helper container
//! ([`crate::oci::helper`]), which also gives whole directory trees in the same process
//! (`<give> <uid>:<gid> <dir>|- <whole> <dir>...`: `-` for no directories-only tree;
//! every entry below each `<whole>` directory, symlinks themselves and never what they
//! point to, by descriptor too).
//!
//! A lease's input files may be hard links to the entries of a shared tree store. A
//! recursive `chown -h` over the input root would re-own every shared inode, so that a
//! job could write it. `chown` has no type filter, so the runner runs this instead: a walk
//! by descriptor, in the style of [`crate::remove`], that opens each directory relative to
//! its parent with `O_NOFOLLOW`, changes its owner, and leaves every other entry (a file,
//! a symlink, a device) as it is. No recursion: a loop over a `Vec` of frames on the
//! heap, one descriptor held at a time, the way back to a parent through `..`, checked by
//! device and inode ([`crate::walk`]).
//!
//! The binary's `main` reaches it through [`crate::modes::dispatch`], before it parses
//! its flags. The argument list is an interface between daemon versions: a node may run
//! a newer binary after an update renamed it over the old one.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Gid, Mode, Uid};

use crate::oci::helper::HelperModes;
use crate::walk::{DIRECTORY, Identity, back_to_parent, identity};

/// What the mode was asked: an owner, the one tree to give directories-only (none for
/// `-`), and the trees to give whole.
struct Request<'a> {
    uid: Uid,
    gid: Gid,
    dirs: Option<&'a OsStr>,
    whole: Vec<&'a OsStr>,
}

/// Reads `<uid>:<gid> <dir>|- [<whole> <dir>...]`, `<whole>` being `whole_word`.
fn parse<'a>(args: &'a [OsString], whole_word: &str) -> Option<Request<'a>> {
    let (owner, rest) = args.split_first()?;
    let (uid, gid) = owner.to_str()?.split_once(':')?;
    let (dirs, rest) = rest.split_first()?;
    let whole = match rest {
        [] => Vec::new(),
        [word, trees @ ..] if word == whole_word && !trees.is_empty() => {
            trees.iter().map(OsString::as_os_str).collect()
        }
        _ => return None,
    };
    Some(Request {
        uid: Uid::from_raw(uid.parse().ok()?),
        gid: Gid::from_raw(gid.parse().ok()?),
        dirs: (dirs != "-").then_some(dirs.as_os_str()),
        whole,
    })
}

/// The mode, its arguments after the mode word (`modes.give`) being
/// `<uid>:<gid> <dir>|- [<whole> <dir>...]`: 0 once every directory is changed (and every
/// entry below each whole tree), 1 when the walk failed, 2 for arguments it cannot read.
#[must_use]
pub fn run(modes: &HelperModes, args: &[OsString]) -> u8 {
    let (mode, whole) = (&modes.give, &modes.whole);
    let Some(request) = parse(args, whole) else {
        eprintln!("{mode}: usage: {mode} <uid>:<gid> <dir>|- [{whole} <dir>...], not {args:?}");
        return 2;
    };
    let (uid, gid) = (request.uid, request.gid);
    if let Some(dir) = request.dirs {
        let done = walk(Path::new(dir), &mut |fd| {
            Ok(rustix::fs::fchown(fd, Some(uid), Some(gid))?)
        });
        if let Err(e) = done {
            eprintln!("{mode}: {}: {e}", Path::new(dir).display());
            return 1;
        }
    }
    for tree in request.whole {
        if let Err(e) = give_whole(Path::new(tree), uid, gid) {
            eprintln!("{mode}: {}: {e}", Path::new(tree).display());
            return 1;
        }
    }
    0
}

/// Gives `dir` and everything below it to `uid:gid`: a directory by its descriptor, any
/// other entry (a symlink itself, never its target) by name in its directory's
/// descriptor, `AT_SYMLINK_NOFOLLOW`.
fn give_whole(dir: &Path, uid: Uid, gid: Gid) -> std::io::Result<()> {
    walk_entries(
        dir,
        &mut |fd| Ok(rustix::fs::fchown(fd, Some(uid), Some(gid))?),
        &mut |parent, name| {
            Ok(rustix::fs::chownat(
                parent,
                name,
                Some(uid),
                Some(gid),
                AtFlags::SYMLINK_NOFOLLOW,
            )?)
        },
    )
}

/// A directory of the walk.
struct Frame {
    id: Identity,
    /// The subdirectories still to visit.
    subdirectories: Vec<OsString>,
}

/// Calls `each` with a descriptor of `dir` and of every directory below it, not
/// following a symlink. An error stops the walk.
pub(crate) fn walk(
    dir: &Path,
    each: &mut dyn FnMut(&OwnedFd) -> std::io::Result<()>,
) -> std::io::Result<()> {
    walk_entries(dir, each, &mut |_, _| Ok(()))
}

/// [`walk`], and `other` called with the descriptor of a directory and the name of each
/// entry in it that is not a directory (a file, a symlink, a device), before the walk
/// goes below that directory.
pub(crate) fn walk_entries(
    dir: &Path,
    each: &mut dyn FnMut(&OwnedFd) -> std::io::Result<()>,
    other: &mut dyn FnMut(&OwnedFd, &OsStr) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut here = rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty())?;
    each(&here)?;
    let mut stack = vec![Frame {
        id: identity(&here)?,
        subdirectories: entries(&here, other)?,
    }];
    while let Some(mut top) = stack.pop() {
        if let Some(name) = top.subdirectories.pop() {
            let child = rustix::fs::openat(&here, name.as_os_str(), DIRECTORY, Mode::empty())?;
            each(&child)?;
            let frame = Frame {
                id: identity(&child)?,
                subdirectories: entries(&child, other)?,
            };
            stack.extend([top, frame]);
            here = child;
            continue;
        }
        if let Some(parent) = stack.last() {
            here = back_to_parent(&here, parent.id)?;
        }
    }
    Ok(())
}

/// The names in `dir` that are directories (not symlinks to one); every other name is
/// handed to `other` with `dir`'s descriptor.
fn entries(
    dir: &OwnedFd,
    other: &mut dyn FnMut(&OwnedFd, &OsStr) -> std::io::Result<()>,
) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsString::from_vec(name);
        let stat = rustix::fs::statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            names.push(name);
        } else {
            other(dir, name.as_os_str())?;
        }
    }
    Ok(names)
}

#[cfg(test)]
#[path = "give_tests.rs"]
mod tests;
