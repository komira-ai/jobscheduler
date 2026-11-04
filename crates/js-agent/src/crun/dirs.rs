//! The lease directories the runtime makes or takes: new ones of mode `0755`, and the one
//! the lifecycle made beside the run's own, checked to be empty and in the run's project;
//! the paths of a lease that an older agent kept in the store, and of the cgroups' files.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use js_contain::cgroup::JOB_OWNED_FILES;

use crate::cli_runtime::Chown;

/// Makes the new directory `dir` with the mode `0755` whatever the umask is, setting the mode
/// by the descriptor of what was made.
pub(super) fn make_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir(dir)?;
    set_open_mode(dir)
}

/// Takes the directory `dir` the lifecycle made for the run whose scratch directory is
/// `reference` ([`Runtime::lease_dir`]) and sets its mode to `0755`. It must be there, be an
/// empty directory (not a link) and have the project id `project` reads of `reference`
/// (`None` on both sides, a file system with no project ids, where no quota can be on, is the
/// same project; a read that failed on either side refuses): otherwise it is refused and
/// nothing is made, so a lease never lies outside the run's disk bound. Both directories
/// are opened once, without following a link, and what is checked and set is what was opened.
///
/// [`Runtime::lease_dir`]: crate::lifecycle::Runtime::lease_dir
pub(super) fn take_dir(
    dir: &Path,
    reference: &Path,
    project: &dyn Fn(&File) -> io::Result<Option<u32>>,
) -> io::Result<()> {
    let open = open_nofollow(dir)?;
    let held = open_nofollow(reference)?;
    // The listing through the descriptor's own `/proc` link; an entry or a listing that cannot
    // be read counts as something in it.
    let empty = std::fs::read_dir(format!("/proc/self/fd/{}", open.as_raw_fd()))
        .is_ok_and(|mut entries| entries.next().is_none());
    if !empty {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an empty directory", dir.display()),
        ));
    }
    if project(&open)? != project(&held)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not in the run's project", dir.display()),
        ));
    }
    fchmod_open(&open)
}

/// The project id of an open directory ([`crate::quota::project_of`]).
pub(super) fn project_of(dir: &File) -> io::Result<Option<u32>> {
    crate::quota::project_of(dir)
}

/// Opens the directory `dir` without following a link.
fn open_nofollow(dir: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::open(dir, flags, Mode::empty())
        .map(File::from)
        .map_err(io::Error::from)
}

/// Sets the mode of the open directory to `0755`.
fn fchmod_open(dir: &File) -> io::Result<()> {
    rustix::fs::fchmod(dir, rustix::fs::Mode::from_raw_mode(0o755)).map_err(io::Error::from)
}

/// Sets the mode of the directory `dir` to `0755` by its descriptor, not following a link.
fn set_open_mode(dir: &Path) -> io::Result<()> {
    open_nofollow(dir).and_then(|fd| fchmod_open(&fd))
}

/// Where an older agent kept the lease `name` in `store`; `None` for a name that is not
/// exactly one ordinary path component (empty, with a `/`, `.` or `..`), which names no lease
/// and must never reach a path.
pub(super) fn legacy_lease(store: &Path, name: &str) -> Option<PathBuf> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(part)), None) if part == name => Some(store.join(name)),
        _ => None,
    }
}

/// The path of `file` of the cgroup `dir` in [`JOB_OWNED_FILES`], where the empty name is the
/// directory itself (not `dir/`, whose trailing slash would make a no-follow open follow).
pub(super) fn owned(dir: &Path, file: &str) -> PathBuf {
    if file.is_empty() {
        dir.to_owned()
    } else {
        dir.join(file)
    }
}

/// Makes the cgroup `dir` of the helper containers (once: one that is there is kept), hands
/// the controllers to its children and gives it to the job user.
pub(super) fn make_helper_cgroup(
    dir: &Path,
    chown: Chown,
    (uid, gid): (u32, u32),
) -> io::Result<()> {
    if !dir.is_dir() {
        std::fs::create_dir(dir)?;
    }
    std::fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    JOB_OWNED_FILES
        .iter()
        .try_for_each(|file| chown(&dir.join(file), uid, gid))
}

/// The directory of the cgroup `name` (relative to `mount`, starting with `/`).
pub(super) fn cgroup_dir(mount: &Path, name: &str) -> PathBuf {
    mount.join(name.trim_start_matches('/'))
}
