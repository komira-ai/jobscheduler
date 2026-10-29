//! The per-run scratch directories, below one root only their owner may enter.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, chmodat, fstat, openat, statat, unlinkat,
};
use rustix::io::Errno;

/// The canonical path of `path`, which must be a directory (not a link to one) that the
/// user running the agent owns, with no permission for group or others. `flag` names the
/// flag in the error.
///
/// # Errors
///
/// The path is not such a directory. The text starts with `flag`.
pub(super) fn private_dir(path: &Path, flag: &str) -> Result<PathBuf, String> {
    let fail = |why: &str| format!("{flag}: {why}");
    let (meta, canonical) = fs::symlink_metadata(path)
        .and_then(|meta| fs::canonicalize(path).map(|canonical| (meta, canonical)))
        .map_err(|_| fail("cannot read the directory"))?;
    let mine = meta.uid() == rustix::process::geteuid().as_raw();
    let owner_only = meta.mode() & 0o077 == 0;
    match (meta.is_dir(), mine, owner_only) {
        (false, _, _) => Err(fail("not a directory (a link is not accepted)")),
        (true, false, _) => Err(fail("not owned by the user running the agent")),
        (true, true, false) => Err(fail("group and others must have no permission")),
        (true, true, true) => Ok(canonical),
    }
}

/// The directory under which every run gets a scratch directory of its own, `0700`.
///
/// The root must be a directory (not a link to one) that the user running the agent
/// owns, with no permission for group or others: a scratch directory is only as private
/// as the directory above it. The agent checks that when it starts and does not change
/// the root. The path is kept in its canonical form (every link on the way resolved), so
/// the path a run's directory is bound by is the one the engine reports for it when the
/// mounts are read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRoot {
    path: PathBuf,
}

impl ScratchRoot {
    /// Opens the root at `path`.
    ///
    /// # Errors
    ///
    /// The path is not an existing directory, is a link, is owned by another user, or
    /// grants group or others any permission. The text names the flag.
    pub fn open(path: &Path) -> Result<Self, String> {
        private_dir(path, "--scratch-root").map(|path| Self { path })
    }

    /// The canonical path of the root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory of the run, which need not exist.
    pub fn of(&self, run_id: &str) -> PathBuf {
        self.path.join(run_id)
    }

    /// Makes the run's directory, mode `0700` whatever the umask. A directory left by an
    /// earlier agent for this run id is removed first: the run is new, so it is garbage.
    ///
    /// # Errors
    ///
    /// The stale directory cannot be removed or the new one cannot be made.
    pub fn create(&self, run_id: &str) -> io::Result<PathBuf> {
        self.remove(run_id)?;
        let dir = self.of(run_id);
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    /// Removes the run's directory and everything in it, whatever the job left: a
    /// directory it closed (mode 0, or without write permission) is opened up first by
    /// restoring the owner's permission, and the tree is walked without recursion, so its
    /// depth is not bounded by the stack or by the length of a path. One that is not there
    /// is not an error. Links inside are removed, never followed and never changed.
    ///
    /// Restoring a mode on what another user owns needs the privilege to do that, which
    /// the agent has when it runs as root.
    ///
    /// A directory on another device than the root (a mount left under it) is not entered
    /// and is an error, so that a mount is never emptied.
    ///
    /// # Errors
    ///
    /// The run id is not one ordinary path component, the run's path is not a directory,
    /// or something in it cannot be removed.
    pub fn remove(&self, run_id: &str) -> io::Result<()> {
        let normal = !matches!(run_id, "" | "." | "..") && !run_id.contains('/');
        let name = CString::new(run_id)
            .ok()
            .filter(|_| normal)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let root = self.open_root()?;
        match remove_tree(&root, &name) {
            Err(Errno::NOENT) | Ok(()) => Ok(()),
            Err(why) => Err(why.into()),
        }
    }

    /// Removes every entry of the root that is not the directory of a run in `kept`, and
    /// returns how many it removed. This is for the start of the agent, which owns the
    /// root: what an earlier agent left for a run that no longer has a container is
    /// garbage. An entry that cannot be removed stays; it is tried again when its run id
    /// is made again, which fails the start of that run.
    pub fn sweep<'a>(&self, kept: impl IntoIterator<Item = &'a str>) -> usize {
        let kept: Vec<&str> = kept.into_iter().collect();
        self.sweep_except(&kept).unwrap_or_default()
    }

    /// [`ScratchRoot::sweep`], or the error that the root could not be listed.
    fn sweep_except(&self, kept: &[&str]) -> io::Result<usize> {
        let (root, entries) = self.open_root().and_then(|root| {
            Dir::read_from(&root)
                .map(|entries| (root, entries))
                .map_err(io::Error::from)
        })?;
        let orphans: Vec<CString> = entries
            .flatten()
            .map(|entry| entry.file_name().to_owned())
            .filter(|name| {
                let bytes = name.to_bytes();
                bytes != b"." && bytes != b".." && !kept.iter().any(|run| run.as_bytes() == bytes)
            })
            .collect();
        // An entry that cannot be examined is unlinked like a file: if it vanished, the
        // unlink fails and it is not counted.
        Ok(orphans
            .iter()
            .filter(|name| match is_dir(&root, name) {
                Ok(true) => remove_tree(&root, name).is_ok(),
                _ => unlinkat(&root, name.as_c_str(), AtFlags::empty()).is_ok(),
            })
            .count())
    }

    fn open_root(&self) -> io::Result<OwnedFd> {
        Ok(openat(CWD, &self.path, DIR_FLAGS, Mode::empty())?)
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const PATH_FLAGS: OFlags = OFlags::PATH
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Whether the entry `name` of `dir` is a directory (not a link to one).
fn is_dir(dir: &OwnedFd, name: &CStr) -> rustix::io::Result<bool> {
    let meta = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(FileType::from_raw_mode(meta.st_mode) == FileType::Directory)
}

/// A directory on another device than the one the removal started on is a mount.
fn same_device(root: u64, found: u64) -> rustix::io::Result<()> {
    match root == found {
        true => Ok(()),
        false => Err(Errno::XDEV),
    }
}

/// `..` must lead back to the directory the walk left (device and inode).
fn same_place(seen: (u64, u64), want: (u64, u64)) -> rustix::io::Result<()> {
    match seen == want {
        true => Ok(()),
        false => Err(Errno::STALE),
    }
}

/// Opens the directory `name` of `parent`, which must be a directory and not a link, on
/// the device `dev`, first giving its owner read, write and search permission when it
/// lacks any. The entry is opened once, without following a link and without needing
/// any permission on it (`O_PATH`); the mode is then changed through that open file, by
/// its `/proc/self/fd` name, so no name that could be swapped is resolved again.
fn open_dir(parent: &OwnedFd, name: &CStr, dev: u64) -> rustix::io::Result<OwnedFd> {
    let entry = openat(parent, name, PATH_FLAGS, Mode::empty())?;
    let meta = fstat(&entry)?;
    same_device(dev, meta.st_dev)?;
    if meta.st_mode & 0o700 != 0o700 {
        let via = format!("/proc/self/fd/{}", entry.as_raw_fd());
        chmodat(CWD, via.as_str(), Mode::RWXU, AtFlags::empty())?;
    }
    openat(&entry, c".", DIR_FLAGS, Mode::empty())
}

/// Removes the files in `dir` and returns the name of one of its subdirectories, if any.
fn clear_files(dir: &OwnedFd) -> rustix::io::Result<Option<CString>> {
    // An entry that cannot be read is skipped. It is then still in the directory, so the
    // removal of the directory fails (not empty) and the removal as a whole fails safe.
    let names: Vec<CString> = Dir::read_from(dir)?
        .flatten()
        .map(|entry| entry.file_name().to_owned())
        .collect();
    let mut subdir = None;
    for name in names {
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        match is_dir(dir, &name)? {
            true => subdir = subdir.or(Some(name)),
            false => unlinkat(dir, name.as_c_str(), AtFlags::empty())?,
        }
    }
    Ok(subdir)
}

/// Removes the directory `name` of `parent` and everything under it. The walk keeps one
/// open directory, and the name and identity of each directory above it; it goes down
/// into a subdirectory, and goes back up through `..` once a directory is empty,
/// checking that `..` is the directory it came from (one moved out of the tree
/// meanwhile is an error, not a place to delete in).
fn remove_tree(parent: &OwnedFd, name: &CStr) -> rustix::io::Result<()> {
    let dev = fstat(parent)?.st_dev;
    let mut cur = open_dir(parent, name, dev)?;
    let mut trail: Vec<(CString, u64, u64)> = Vec::new();
    loop {
        if let Some(sub) = clear_files(&cur)? {
            let here = fstat(&cur)?;
            let below = open_dir(&cur, &sub, dev)?;
            trail.push((sub, here.st_dev, here.st_ino));
            cur = below;
            continue;
        }
        let Some((done, up_dev, up_ino)) = trail.pop() else {
            return unlinkat(parent, name, AtFlags::REMOVEDIR);
        };
        let up = openat(&cur, c"..", DIR_FLAGS, Mode::empty())?;
        let seen = fstat(&up)?;
        same_place((seen.st_dev, seen.st_ino), (up_dev, up_ino))?;
        unlinkat(&up, done.as_c_str(), AtFlags::REMOVEDIR)?;
        cur = up;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory on the device the removal started on is entered; one on another
    /// device (a mount) is refused with a cross-device error.
    #[test]
    fn a_directory_on_another_device_is_refused() {
        assert_eq!(same_device(7, 7), Ok(()));
        assert_eq!(same_device(7, 8), Err(Errno::XDEV));
    }

    /// `..` that leads to the directory the walk left is accepted; one that leads
    /// anywhere else (another inode, or another device) is stale.
    #[test]
    fn dot_dot_must_lead_back_to_the_directory_left() {
        assert_eq!(same_place((1, 2), (1, 2)), Ok(()));
        assert_eq!(same_place((1, 3), (1, 2)), Err(Errno::STALE));
        assert_eq!(same_place((2, 2), (1, 2)), Err(Errno::STALE));
    }
}
