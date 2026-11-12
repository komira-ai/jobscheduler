//! Opening a host path beneath the directory it is allowed under, without a race: the
//! kernel resolves every part of it, links included, and refuses one that leads out.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// A directory opened for its name alone (`O_PATH`): it can be resolved beneath and cloned,
/// not read.
const PATH_DIRECTORY: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);

/// How many times [`open_beneath`] calls `openat2` for one path while the kernel answers
/// `EAGAIN` (a rename or a mount somewhere on the host raced the resolution of a `..`, and
/// the kernel asks the caller to try again) before that answer is the path's. The calls
/// follow each other with no wait: each is a fresh resolution, and the host's renames and
/// mounts are the only thing it waits on.
pub(super) const RESOLVE_ATTEMPTS: usize = 16;

/// Why a path could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The path leads out of the directory it is opened beneath: an absolute path, a `..`
    /// above it, or a link (absolute, or relative and climbing) that does (`EXDEV`).
    #[error("{} leads out of the directory it is opened beneath", path.display())]
    Escapes {
        /// The path, as given.
        path: PathBuf,
    },
    /// The path is missing, is not a directory, cannot be searched, or could not be
    /// resolved for another reason the kernel gave.
    #[error("{}: {source}", path.display())]
    Unavailable {
        /// The path, as given.
        path: PathBuf,
        /// The kernel's answer.
        #[source]
        source: io::Error,
    },
}

/// Opens the directory `path` (`O_PATH | O_DIRECTORY`, links followed), to open paths
/// beneath ([`open_beneath`]).
///
/// # Errors
/// [`ResolveError::Unavailable`]: `path` is missing, not a directory or cannot be searched.
pub fn open_root(path: &Path) -> Result<OwnedFd, ResolveError> {
    rustix::fs::openat(CWD, path, PATH_DIRECTORY, Mode::empty()).map_err(|errno| {
        ResolveError::Unavailable {
            path: path.to_owned(),
            source: errno.into(),
        }
    })
}

/// Opens the directory `rest`, a relative path, beneath the directory open as `dir`
/// (`openat2` with `O_PATH | O_DIRECTORY` and `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`): a
/// link in it may lead anywhere inside `dir` and nowhere outside. An empty `rest` opens
/// `dir` itself. A file system mounted below `dir` may be crossed (`RESOLVE_NO_XDEV` is not
/// given). The descriptor names the directory found then, whatever later happens to the
/// path.
///
/// A resolution the kernel answers `EAGAIN`, because a rename or a mount anywhere on the
/// host raced a `..` in it, is made again, up to `RESOLVE_ATTEMPTS` (16) times in all, with no
/// wait between.
///
/// # Errors
/// [`ResolveError::Escapes`] for a path that leads out of `dir`; otherwise
/// [`ResolveError::Unavailable`] with the kernel's answer (`ENOENT`, `ENOTDIR`, `EACCES`,
/// `ELOOP` for a link loop or a magic link, `EAGAIN` when renames or mounts raced every
/// attempt).
pub fn open_beneath(dir: BorrowedFd<'_>, rest: &Path) -> Result<OwnedFd, ResolveError> {
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
    resolve_with(rest, &mut |path| {
        rustix::fs::openat2(dir, path, PATH_DIRECTORY, Mode::empty(), resolve)
    })
}

/// [`open_beneath`] with its `openat2` given as `openat2`, which is called with the path
/// to resolve: `rest`, or `.` for an empty `rest`.
pub(super) fn resolve_with(
    rest: &Path,
    openat2: &mut dyn FnMut(&Path) -> Result<OwnedFd, Errno>,
) -> Result<OwnedFd, ResolveError> {
    let path = if rest.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rest
    };
    let mut left = RESOLVE_ATTEMPTS;
    let answer = loop {
        left -= 1;
        match openat2(path) {
            Err(Errno::AGAIN) if left > 0 => {}
            answer => break answer,
        }
    };
    answer.map_err(|errno| match errno {
        Errno::XDEV => ResolveError::Escapes {
            path: rest.to_owned(),
        },
        other => ResolveError::Unavailable {
            path: rest.to_owned(),
            source: other.into(),
        },
    })
}
