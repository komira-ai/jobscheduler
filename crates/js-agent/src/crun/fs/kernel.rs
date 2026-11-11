//! The calls of a job's file system that look at a provider's file system, and so may wait
//! on it: [`PathCalls`], which the agent makes through [`Kernel`] on a thread of the
//! `js-paths` pool and a test replaces, and the making of a mount target inside a tree
//! ([`reach`]).

use std::fmt;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use js_contain::job_fs::{ResolveError, open_beneath};
use rustix::fs::{Access, AtFlags, CWD, Mode, OFlags, ResolveFlags};
use rustix::process::{Gid, Uid};

/// The calls of a job's file system that read a host path.
pub trait PathCalls: Send + Sync + fmt::Debug {
    /// Opens the directory `rest` beneath the directory open as `dir`
    /// ([`js_contain::job_fs::open_beneath`]): a link that leads out of `dir` is refused.
    ///
    /// # Errors
    /// See [`ResolveError`].
    fn open_beneath(&self, dir: BorrowedFd<'_>, rest: &Path) -> Result<OwnedFd, ResolveError>;

    /// Whether the user `ids` (uid, gid, and no other group) may search the directory
    /// `dir`: `faccessat(X_OK, AT_EACCESS)` on a thread of its own that takes those ids
    /// for good and ends.
    ///
    /// # Errors
    /// The user may not search it, or the ids could not be taken.
    fn search_as(&self, dir: &Path, ids: (u32, u32)) -> io::Result<()>;

    /// Whether the directory `dir` answers: it is opened, its file system's statistics read
    /// (`fstatfs`) and one entry of it listed (`getdents`).
    ///
    /// # Errors
    /// What the file system answered.
    fn answers(&self, dir: &Path) -> io::Result<()>;

    /// Gives `name` in the directory open as `dir` to `ids` (`fchownat`, following no link).
    ///
    /// # Errors
    /// The kernel's.
    fn give_at(
        &self,
        dir: BorrowedFd<'_>,
        name: &std::ffi::OsStr,
        ids: (u32, u32),
    ) -> io::Result<()>;
}

/// The calls themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct Kernel;

impl PathCalls for Kernel {
    fn open_beneath(&self, dir: BorrowedFd<'_>, rest: &Path) -> Result<OwnedFd, ResolveError> {
        open_beneath(dir, rest)
    }

    fn search_as(&self, dir: &Path, ids: (u32, u32)) -> io::Result<()> {
        search_with(dir, ids, become_user)
    }

    fn answers(&self, dir: &Path) -> io::Result<()> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let opened = rustix::fs::open(dir, flags, Mode::empty())?;
        rustix::fs::fstatfs(&opened)?;
        let mut entries = rustix::fs::Dir::read_from(&opened)?;
        entries
            .read()
            .transpose()
            .map(drop)
            .map_err(io::Error::from)
    }

    fn give_at(
        &self,
        dir: BorrowedFd<'_>,
        name: &std::ffi::OsStr,
        (uid, gid): (u32, u32),
    ) -> io::Result<()> {
        let (uid, gid) = (Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)));
        rustix::fs::chownat(dir, name, uid, gid, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)
    }
}

/// How the thread of [`search_with`] takes the ids it searches as: [`become_user`] on a node.
pub(super) type Become = fn((u32, u32)) -> io::Result<()>;

/// Makes the calling thread, and it alone, the user `ids` for good: no supplementary group,
/// then the gid, then the uid, each real, effective and saved. Every call is made, so the
/// first refusal (a caller that is not root) is the answer.
///
/// # Errors
/// The first call the kernel refused.
pub(super) fn become_user((uid, gid): (u32, u32)) -> io::Result<()> {
    let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
    rustix::thread::set_thread_groups(&[])
        .and(rustix::thread::set_thread_res_gid(gid, gid, gid))
        .and(rustix::thread::set_thread_res_uid(uid, uid, uid))
        .map_err(io::Error::from)
}

/// [`PathCalls::search_as`], with a thread that takes the ids with `take_ids`.
///
/// # Errors
/// The thread could not be made, the ids could not be taken, or the user may not search
/// `dir`.
pub(super) fn search_with(dir: &Path, ids: (u32, u32), take_ids: Become) -> io::Result<()> {
    let mut searched = Ok(());
    // The scope joins the thread before it ends, and a panic of the thread is the caller's.
    let spawned = std::thread::scope(|scope| {
        let thread = std::thread::Builder::new().name("js-paths-as".to_owned());
        let made = thread.spawn_scoped(scope, || {
            searched = take_ids(ids).and_then(|()| {
                rustix::fs::accessat(CWD, dir, Access::EXEC_OK, AtFlags::EACCESS)
                    .map_err(io::Error::from)
            });
        });
        made.map(drop)
    });
    spawned.and(searched)
}

/// Whether `why`, the answer of [`PathCalls::answers`], says the file system has gone or
/// cannot be read (`ENOTCONN`: a FUSE file system whose server is gone; `EIO`; `ESTALE`),
/// rather than something about the directory itself.
pub(super) fn unreadable(why: &io::Error) -> bool {
    matches!(
        why.raw_os_error(),
        Some(libc::ENOTCONN | libc::EIO | libc::ESTALE)
    )
}

/// The flags of a directory opened for its name alone.
const PATH_DIRECTORY: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);

/// Makes sure the directory `rel` (relative, in normal form) is in the tree at `tree`, as
/// the tree's own links lead (`RESOLVE_IN_ROOT`: a link is read inside the tree, as the
/// job will read it, never out of it). With `owner`, each part that is not there is made,
/// mode `0755`, and given to `owner` by `paths`; without it, a part that is not there is an
/// error.
///
/// # Errors
/// A part is not there (without `owner`), is not a directory, or could not be made or
/// given away.
pub(super) fn reach(
    tree: &Path,
    rel: &Path,
    owner: Option<(u32, u32)>,
    paths: &dyn PathCalls,
) -> io::Result<()> {
    let root = rustix::fs::open(tree, PATH_DIRECTORY, Mode::empty())?;
    let mut at = PathBuf::new();
    for part in rel.components() {
        let parent = in_root(&root, &at)?;
        at.push(part);
        match (in_root(&root, &at), owner) {
            (Ok(_), _) => {}
            (Err(why), Some(ids)) if why.raw_os_error() == Some(libc::ENOENT) => {
                make_owned(&parent, part.as_os_str(), ids, paths)?;
            }
            (Err(why), _) => return Err(why),
        }
    }
    Ok(())
}

/// The directory `rel` of the tree open as `root`, opened for its name alone, its links
/// read inside the tree; the tree itself for an empty `rel`.
fn in_root(root: &OwnedFd, rel: &Path) -> io::Result<OwnedFd> {
    let rel = match rel.as_os_str().is_empty() {
        true => Path::new("."),
        false => rel,
    };
    let resolve = ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS;
    rustix::fs::openat2(root.as_fd(), rel, PATH_DIRECTORY, Mode::empty(), resolve)
        .map_err(io::Error::from)
}

/// Makes the directory `name` in the directory open as `parent`, mode `0755` whatever the
/// umask, and gives it to `ids` by `paths`.
fn make_owned(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    ids: (u32, u32),
    paths: &dyn PathCalls,
) -> io::Result<()> {
    let mode = Mode::from_raw_mode(0o755);
    rustix::fs::mkdirat(parent, name, mode)?;
    rustix::fs::chmodat(parent, name, mode, AtFlags::empty())?;
    paths.give_at(parent.as_fd(), name, ids)
}

/// Makes the directory `name` in `parent` (and `parent`, when it is not there), with `mode`
/// whatever the umask, and gives it to `ids` by `paths`: a writable layer of an assembly or
/// its work directory, in the lease directory.
pub(super) fn make_layer_dir(
    (parent, name): (&Path, &str),
    mode: u32,
    ids: (u32, u32),
    paths: &dyn PathCalls,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = parent.join(name);
    super::list::make_dir(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))?;
    let opened = rustix::fs::open(parent, PATH_DIRECTORY, Mode::empty())?;
    paths.give_at(opened.as_fd(), name.as_ref(), ids)
}

/// Whether `answer` is the kernel's answer that a call does not exist (`ENOSYS`).
pub(crate) fn missing_call(answer: &io::Result<()>) -> bool {
    answer
        .as_ref()
        .is_err_and(|why| why.raw_os_error() == Some(libc::ENOSYS))
}
