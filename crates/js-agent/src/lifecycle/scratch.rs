//! The per-run scratch directories, below one root only their owner may enter.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Dev, Dir, FileType, Mode, OFlags, chmodat, fstat, openat, statat, unlinkat,
};
use rustix::io::Errno;

/// Who besides the agent can reach the directories the agent makes for jobs.
///
/// The podman path runs the container engine as root, which reaches every directory, so
/// the roots are the agent's alone ([`DirLayout::Private`]). The rootless crun path binds
/// the directories into the job from an account that is not the agent's (the job's host
/// ids), and that account must be able to walk down to them ([`DirLayout::Searchable`]).
/// The layout is chosen by the runtime that is in use; the default is the podman rule, so
/// nothing changes for an agent that does not ask for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DirLayout {
    /// The root has no permission bit for group or others (`0700`), and so has each
    /// directory made below it.
    #[default]
    Private,
    /// The root and each directory made below it can be searched, and nothing more, by
    /// others (`0711`: others get `x`, never `r` or `w`, so a job cannot list the names of
    /// the other runs), and so can every directory above the root (the rule of the
    /// container runner's own start check). What a job may read inside a run's
    /// directory is decided by the owner and mode of what the agent puts there.
    Searchable,
}

impl DirLayout {
    /// The mode of the root and of each run's directory in this layout.
    pub const fn dir_mode(self) -> u32 {
        match self {
            Self::Private => 0o700,
            Self::Searchable => 0o711,
        }
    }

    /// Whether `mode` (the permission bits of an existing root) is acceptable.
    pub(crate) const fn allows(self, mode: u32) -> bool {
        match self {
            Self::Private => mode & 0o077 == 0,
            Self::Searchable => mode & 0o066 == 0 && mode & 0o001 != 0 && mode & 0o700 == 0o700,
        }
    }

    /// Why a root whose mode `allows` refuses is refused.
    pub(crate) const fn refusal(self) -> &'static str {
        match self {
            Self::Private => "group and others must have no permission",
            Self::Searchable => {
                "others must be able to search the directory (x) and group and others must have no read or write permission (0711)"
            }
        }
    }

    /// Whether every directory above `canonical` can be searched by others, which the
    /// job's ids need to walk down to the root. The check reads the mode bits, as the
    /// container runner's start check does; it is a no-op for [`DirLayout::Private`].
    fn require_reachable(self, canonical: &Path) -> Result<(), String> {
        if self == Self::Private {
            return Ok(());
        }
        for ancestor in canonical.ancestors().skip(1) {
            // A directory that cannot be looked at cannot be searched either.
            let searchable = fs::metadata(ancestor).is_ok_and(|meta| meta.mode() & 0o001 != 0);
            if !searchable {
                return Err(format!(
                    "{} is not searchable by others: the job's user could not reach a directory below it",
                    ancestor.display()
                ));
            }
        }
        Ok(())
    }
}

/// The canonical path of `path`, which must be a directory (not a link to one) that the
/// user running the agent owns, with no permission for group or others. `flag` names the
/// flag in the error.
///
/// # Errors
///
/// The path is not such a directory. The text starts with `flag`.
pub(super) fn private_dir(path: &Path, flag: &str) -> Result<PathBuf, String> {
    layout_dir(path, flag, DirLayout::Private)
}

/// [`private_dir`] for a root of the given `layout`.
///
/// # Errors
///
/// The path is not a directory (not a link to one) that the user running the agent owns
/// with the permissions `layout` allows, or (for [`DirLayout::Searchable`]) a directory
/// above it cannot be searched by others. The text starts with `flag`.
pub(super) fn layout_dir(path: &Path, flag: &str, layout: DirLayout) -> Result<PathBuf, String> {
    let fail = |why: &str| format!("{flag}: {why}");
    let (meta, canonical) = fs::symlink_metadata(path)
        .and_then(|meta| fs::canonicalize(path).map(|canonical| (meta, canonical)))
        .map_err(|_| fail("cannot read the directory"))?;
    let mine = meta.uid() == rustix::process::geteuid().as_raw();
    let allowed = layout.allows(meta.mode());
    match (meta.is_dir(), mine, allowed) {
        (false, _, _) => Err(fail("not a directory (a link is not accepted)")),
        (true, false, _) => Err(fail("not owned by the user running the agent")),
        (true, true, false) => Err(fail(layout.refusal())),
        (true, true, true) => layout
            .require_reachable(&canonical)
            .map(|()| canonical)
            .map_err(|why| fail(&why)),
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
    layout: DirLayout,
}

impl ScratchRoot {
    /// The lock file the OCI backend holds in the root while it lives
    /// (`js_contain::oci::lock::LOCK_FILE`, which Linux builds check this against). No
    /// run id starts with a dot, and a sweep keeps it.
    pub const LOCK_FILE: &'static str = ".js-oci.lock";

    /// Opens the root at `path`.
    ///
    /// # Errors
    ///
    /// The path is not an existing directory, is a link, is owned by another user, or
    /// grants group or others any permission. The text names the flag.
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_with(path, DirLayout::Private)
    }

    /// Opens the root at `path` with the permissions `layout` allows: [`ScratchRoot::open`]
    /// is this with [`DirLayout::Private`].
    ///
    /// # Errors
    ///
    /// The path is not an existing directory, is a link, is owned by another user, has a
    /// mode `layout` does not allow, or (for [`DirLayout::Searchable`]) has an ancestor
    /// others cannot search. The text names the flag.
    pub fn open_with(path: &Path, layout: DirLayout) -> Result<Self, String> {
        layout_dir(path, "--scratch-root", layout).map(|path| Self { path, layout })
    }

    /// The layout the root was opened with.
    pub const fn layout(&self) -> DirLayout {
        self.layout
    }

    /// The canonical path of the root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory of the run, which need not exist.
    pub fn of(&self, run_id: &str) -> PathBuf {
        self.path.join(run_id)
    }

    /// A path directly below the root that names nothing and, as no run id starts with a dot,
    /// never will. The scratch quota's `clear` is given it for a project whose directory a
    /// sweep removed: a directory that is gone is not opened, only the limits come off. If
    /// something did stand there, `clear` opens it with `O_DIRECTORY | O_NOFOLLOW`
    /// (`quota::sys`, `open_dir`), so a link is refused, never followed.
    pub(super) fn vanished(&self) -> PathBuf {
        self.path.join(".js-swept")
    }

    /// The directory the quota's health check makes and removes (see `RunQuota::check`):
    /// directly below the root, and, as no run id starts with a dot, never a run's.
    pub(super) fn health_dir(&self) -> PathBuf {
        self.path.join(".js-health")
    }

    /// Makes the run's directory, mode `0700` whatever the umask (`0711` for a
    /// [`DirLayout::Searchable`] root). A directory left by an earlier agent for this run
    /// id is removed first: the run is new, so it is garbage.
    ///
    /// # Errors
    ///
    /// The stale directory cannot be removed or the new one cannot be made.
    pub fn create(&self, run_id: &str) -> io::Result<PathBuf> {
        self.make_entry(run_id)
    }

    /// Makes the empty directory of the runtime beside the run's directory that `lease` names
    /// (the last component of the path [`Runtime::lease_dir`](super::Runtime::lease_dir)
    /// gives), the way [`ScratchRoot::create`] makes the run's, and returns its path in this
    /// root. One left by an earlier agent is removed first.
    ///
    /// # Errors
    ///
    /// The path has no name that is one ordinary path component, the stale directory cannot
    /// be removed, or the new one cannot be made.
    pub fn create_lease(&self, lease: &Path) -> io::Result<PathBuf> {
        self.make_entry(lease_name(lease)?)
    }

    /// Removes the directory [`ScratchRoot::create_lease`] makes for `lease`, with everything
    /// in it, as [`ScratchRoot::remove`] does; one that is not there is not an error.
    ///
    /// # Errors
    ///
    /// As [`ScratchRoot::remove`].
    pub fn remove_lease(&self, lease: &Path) -> io::Result<()> {
        remove_entry(&self.path, lease_name(lease)?)
    }

    fn make_entry(&self, name: &str) -> io::Result<PathBuf> {
        remove_entry(&self.path, name)?;
        let dir = self.path.join(name);
        let mode = self.layout.dir_mode();
        fs::DirBuilder::new().mode(mode).create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode))?;
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
        remove_entry(&self.path, run_id)
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
        // The lock of the OCI backend on this directory is not a run's, but must stay: it
        // is the lock the agent itself holds, and a new file would lock nothing.
        sweep_entries(&self.path, &|name| {
            name == Self::LOCK_FILE.as_bytes() || kept.iter().any(|run| run.as_bytes() == name)
        })
    }
}

/// The name of the directory `lease` as an entry of the scratch root.
fn lease_name(lease: &Path) -> io::Result<&str> {
    lease
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

fn open_root(path: &Path) -> io::Result<OwnedFd> {
    Ok(openat(CWD, path, DIR_FLAGS, Mode::empty())?)
}

/// Removes the entry `run_id` of the directory `root` with everything under it (see
/// [`ScratchRoot::remove`]); one that is not there is not an error.
pub(super) fn remove_entry(root: &Path, run_id: &str) -> io::Result<()> {
    let normal = !matches!(run_id, "" | "." | "..") && !run_id.contains('/');
    let name = CString::new(run_id)
        .ok()
        .filter(|_| normal)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let root = open_root(root)?;
    match remove_tree(&root, &name) {
        Err(Errno::NOENT) | Ok(()) => Ok(()),
        Err(why) => Err(why.into()),
    }
}

/// Removes every entry of the directory `path` that `keep` does not name, and returns how
/// many it removed (see [`ScratchRoot::sweep`]).
pub(super) fn sweep_entries(path: &Path, keep: &dyn Fn(&[u8]) -> bool) -> io::Result<usize> {
    let (root, entries) = open_root(path).and_then(|root| {
        Dir::read_from(&root)
            .map(|entries| (root, entries))
            .map_err(io::Error::from)
    })?;
    let orphans: Vec<CString> = entries
        .flatten()
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| {
            let bytes = name.to_bytes();
            bytes != b"." && bytes != b".." && !keep(bytes)
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

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How a directory entry is opened before its mode is known: on Linux without needing any
/// permission on it (`O_PATH`).
#[cfg(target_os = "linux")]
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
fn same_device(root: Dev, found: Dev) -> rustix::io::Result<()> {
    match root == found {
        true => Ok(()),
        false => Err(Errno::XDEV),
    }
}

/// `..` must lead back to the directory the walk left (device and inode).
fn same_place(seen: (Dev, u64), want: (Dev, u64)) -> rustix::io::Result<()> {
    match seen == want {
        true => Ok(()),
        false => Err(Errno::STALE),
    }
}

/// Opens the directory `name` of `parent`, which must be a directory and not a link, on
/// the device `dev`, first giving its owner read, write and search permission when it
/// lacks any. On Linux the entry is opened once, without following a link and without
/// needing any permission on it (`O_PATH`); the mode is then changed through that open
/// file, by its `/proc/self/fd` name, so no name that could be swapped is resolved again.
#[cfg(target_os = "linux")]
fn open_dir(parent: &OwnedFd, name: &CStr, dev: Dev) -> rustix::io::Result<OwnedFd> {
    use std::os::fd::AsRawFd;
    let entry = openat(parent, name, PATH_FLAGS, Mode::empty())?;
    let meta = fstat(&entry)?;
    same_device(dev, meta.st_dev)?;
    if meta.st_mode & 0o700 != 0o700 {
        let via = format!("/proc/self/fd/{}", entry.as_raw_fd());
        chmodat(CWD, via.as_str(), Mode::RWXU, AtFlags::empty())?;
    }
    openat(&entry, c".", DIR_FLAGS, Mode::empty())
}

/// [`open_dir`] where there is no `O_PATH` and no `/proc/self/fd` (macOS): the entry is
/// looked at without following a link, its mode is changed by name without following a
/// link (`AT_SYMLINK_NOFOLLOW`), and it is opened with `O_NOFOLLOW|O_DIRECTORY`, so a link
/// swapped in between is never followed: it fails the open. Container jobs, the reason
/// for the Linux form, run only on Linux.
#[cfg(not(target_os = "linux"))]
fn open_dir(parent: &OwnedFd, name: &CStr, dev: Dev) -> rustix::io::Result<OwnedFd> {
    let meta = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(meta.st_mode) != FileType::Directory {
        return Err(Errno::NOTDIR);
    }
    same_device(dev, meta.st_dev)?;
    if meta.st_mode & 0o700 != 0o700 {
        chmodat(parent, name, Mode::RWXU, AtFlags::SYMLINK_NOFOLLOW)?;
    }
    openat(parent, name, DIR_FLAGS, Mode::empty())
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
    let mut trail: Vec<(CString, Dev, u64)> = Vec::new();
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

    /// A searchable root must still be the agent's to read, write and search (`rwx` for the
    /// owner), besides others' search bit and no group or other read or write. It catches
    /// the 0o001 test alone accepting a root the agent itself cannot use (0o311, 0o611,
    /// 0o001) and a private root being taken for a searchable one.
    #[test]
    fn a_searchable_root_needs_the_owner_to_have_all_of_rwx() {
        assert!(DirLayout::Searchable.allows(0o711));
        for bad in [
            0o311, 0o611, 0o511, 0o710, 0o001, 0o755, 0o717, 0o771, 0o700,
        ] {
            assert!(!DirLayout::Searchable.allows(bad), "{bad:o}");
        }
        assert!(DirLayout::Private.allows(0o700));
        assert!(!DirLayout::Private.allows(0o711));
    }

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
