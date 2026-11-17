//! Binding host directories into the mount namespace of a container that is already
//! running, from a root daemon: the way `systemd` binds a mount into a service's namespace
//! (`bind_mount_in_namespace`) with the new mount API. A warm sandbox is a container made
//! and started before its job; when the job arrives the daemon binds the job's scratch
//! directory, writable directories and mounts into it with [`bind_into`], and then tells
//! the container's init to run the program.
//!
//! Two stages, so that a source that cannot be bound fails before the container is
//! touched:
//!
//! 1. In the daemon's own namespace, each source is opened with `O_PATH | O_NOFOLLOW`,
//!    cloned into a detached mount ([`MountCalls::clone_tree`]) and set
//!    ([`MountCalls::set_attributes`]): `nosuid`, `nodev`, private, and read-only or
//!    `noexec` as asked, all the way down.
//! 2. One thread of its own then opens the container's mount namespace and root through
//!    `/proc/<pid>/`, gives itself its own file system attributes (`CLONE_FS`), joins the
//!    namespace and makes the container's root its own root. It makes each target's missing
//!    directories (or file) as the job's user, and attaches the clone there. The thread
//!    has left the daemon's root and namespace and ends when it is done, so nothing of the
//!    daemon runs in the container's view afterwards.

use std::fmt;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use super::{Attributes, MountCalls};
use crate::container::HostMount;

/// The calls that enter a container's mount namespace and make mount points there, one
/// system call each (the decisions are in [`bind_into`], which a fake tests).
pub trait EnterCalls {
    /// `open(path, O_PATH | O_CLOEXEC | O_NOFOLLOW)` in the daemon's namespace: the source,
    /// which a link swapped in for it afterwards cannot change.
    ///
    /// # Errors
    /// The kernel's.
    fn open_source(&self, path: &Path) -> io::Result<OwnedFd>;

    /// `fstat`: whether the opened `source` is a directory.
    ///
    /// # Errors
    /// The kernel's.
    fn is_dir(&self, source: BorrowedFd<'_>) -> io::Result<bool>;

    /// `open("/proc/<pid>/ns/mnt", O_RDONLY | O_CLOEXEC)`: the mount namespace of `pid`.
    ///
    /// # Errors
    /// The kernel's: `ENOENT` when there is no such process.
    fn open_namespace(&self, pid: u32) -> io::Result<OwnedFd>;

    /// `open("/proc/<pid>/root", O_PATH | O_DIRECTORY | O_CLOEXEC)`: the root directory of
    /// `pid`, as that process sees it.
    ///
    /// # Errors
    /// The kernel's: `ENOENT` when there is no such process.
    fn open_root(&self, pid: u32) -> io::Result<OwnedFd>;

    /// `unshare(CLONE_FS)` for the calling thread: its root and working directory are its
    /// own from here on, so that changing them leaves the daemon's other threads alone.
    ///
    /// # Errors
    /// The kernel's.
    fn unshare_fs(&self) -> io::Result<()>;

    /// `setns(namespace, CLONE_NEWNS)`: the calling thread joins the mount namespace.
    ///
    /// # Errors
    /// The kernel's: `EPERM` without the privilege over the namespace's owner.
    fn join(&self, namespace: BorrowedFd<'_>) -> io::Result<()>;

    /// `fchdir(dir)`: the calling thread's working directory is `dir`.
    ///
    /// # Errors
    /// The kernel's.
    fn change_dir(&self, dir: BorrowedFd<'_>) -> io::Result<()>;

    /// `chroot(".")`: the calling thread's working directory is its root.
    ///
    /// # Errors
    /// The kernel's: `EPERM` without `CAP_SYS_CHROOT`.
    fn change_root(&self) -> io::Result<()>;

    /// `setfsgid` then `setfsuid` for the calling thread, to `(user, group)`, so that the
    /// directories it makes are the job user's. Each is checked: the kernel leaves the id
    /// as it was when it refuses, and answers the old id either way, so the call is made a
    /// second time and its answer compared with the id asked for.
    ///
    /// # Errors
    /// `PermissionDenied` when either id did not change to the one asked for.
    fn set_fs_ids(&self, ids: (u32, u32)) -> io::Result<()>;

    /// `mkdir(path, 0755)`; a directory that is there already is no error.
    ///
    /// # Errors
    /// The kernel's.
    fn make_dir(&self, path: &Path) -> io::Result<()>;

    /// `open(path, O_CREAT | O_WRONLY | O_CLOEXEC | O_NOFOLLOW, 0644)`, closed at once; a
    /// file that is there already is no error.
    ///
    /// # Errors
    /// The kernel's.
    fn make_file(&self, path: &Path) -> io::Result<()>;
}

/// What [`bind_into`] needs: the mount calls and the enter calls. The calls are made from
/// a thread of their own, so they are `Sync`.
pub trait Inject: MountCalls + EnterCalls + Sync {}

impl<T: MountCalls + EnterCalls + Sync> Inject for T {}

/// The step of [`bind_into`] that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectStep {
    /// Opening the source ([`EnterCalls::open_source`]).
    OpenSource,
    /// Finding whether the source is a directory ([`EnterCalls::is_dir`]).
    Inspect,
    /// Cloning the source ([`MountCalls::clone_tree`]).
    Clone,
    /// Setting the detached clone ([`MountCalls::set_attributes`]).
    Attributes,
    /// Becoming the job's user in the file system calls ([`EnterCalls::set_fs_ids`]).
    BecomeJob,
    /// Making the mount point or one of its directories ([`EnterCalls::make_dir`],
    /// [`EnterCalls::make_file`]).
    MakeMountPoint,
    /// Becoming the daemon's user again ([`EnterCalls::set_fs_ids`]).
    BecomeRoot,
    /// Attaching the clone ([`MountCalls::attach`]).
    Attach,
}

impl fmt::Display for InjectStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OpenSource => "open the source of",
            Self::Inspect => "find whether the source is a directory, for",
            Self::Clone => "clone the source of",
            Self::Attributes => "set the clone's attributes for",
            Self::BecomeJob => "become the job's user to make",
            Self::MakeMountPoint => "make the mount point",
            Self::BecomeRoot => "become the daemon's user again after making",
            Self::Attach => "attach the clone at",
        })
    }
}

/// What [`bind_into`] could not do.
#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    /// A target is not an absolute, normalized path with at least one part. Nothing was
    /// opened.
    #[error("the target {target:?} is not an absolute, normalized path below /")]
    BadTarget {
        /// The target as given.
        target: String,
    },
    /// A step of binding one source failed.
    #[error("{step} {target}: {source}")]
    Step {
        /// The step that failed.
        step: InjectStep,
        /// The bind's target.
        target: String,
        /// What the kernel answered.
        #[source]
        source: io::Error,
    },
    /// Entering the container's namespace and root failed. Nothing was made there.
    #[error("enter the namespace of process {pid} ({step}): {source}")]
    Enter {
        /// The container's process.
        pid: u32,
        /// The call that failed: `open namespace`, `open root`, `unshare fs`, `join`,
        /// `change dir` or `change root`.
        step: &'static str,
        /// What the kernel answered.
        #[source]
        source: io::Error,
    },
    /// The thread that enters the container panicked. Some binds may be attached.
    #[error("the thread that binds into the namespace of process {pid} panicked")]
    Panicked {
        /// The container's process.
        pid: u32,
    },
}

/// A source cloned and set, waiting to be attached at its target.
struct Prepared<'a> {
    bind: &'a HostMount,
    tree: OwnedFd,
    is_dir: bool,
}

/// The error of `step` for `bind`, as a function of the kernel's answer.
fn failed(step: InjectStep, bind: &HostMount) -> impl FnOnce(io::Error) -> InjectError + '_ {
    move |source| InjectError::Step {
        step,
        target: bind.target.clone(),
        source,
    }
}

/// The error of the entering call `step` on process `pid`.
fn entering(pid: u32, step: &'static str) -> impl FnOnce(io::Error) -> InjectError {
    move |source| InjectError::Enter { pid, step, source }
}

/// Binds each of `binds` into the mount namespace of the process `pid` (a container's first
/// process), in order, at its target as the container sees it: as `crun` would have bound
/// it at the container's creation, a clone that is private, `nosuid` and `nodev`, and
/// read-only or `noexec` as the bind says, all the way down, with every missing directory
/// of the target (and the target, or an empty file for a source that is not a directory)
/// made as the job's user `ids` (user, group), which is a user the container's mounts
/// (a `tmpfs` of its user namespace) have a mapping for, where the daemon's own root has
/// none. The clones are made in the daemon's namespace first, and every one is made before
/// the container is entered, so a source that cannot be bound leaves the container as it
/// was.
///
/// A bind that is not [`recursive`](HostMount::recursive) is cloned recursively too, as
/// the mounts below the source come along with a clone: [`MountCalls::clone_tree`] has no
/// other form. With no binds nothing is entered and the answer is `Ok`.
///
/// The container is entered by one new thread, which leaves the daemon's root and
/// namespace and ends when it is done.
///
/// # Errors
/// A target that is not absolute and normalized ([`InjectError::BadTarget`], before
/// anything is opened); the step that failed for a source or a target
/// ([`InjectError::Step`]); entering the namespace ([`InjectError::Enter`]); a panic of the
/// thread ([`InjectError::Panicked`]). The binds attached before a failure stay: the caller
/// removes the container.
pub fn bind_into(
    calls: &dyn Inject,
    pid: u32,
    binds: &[HostMount],
    ids: (u32, u32),
) -> Result<(), InjectError> {
    if let Some(bad) = binds.iter().find(|bind| !normalized(&bind.target)) {
        return Err(InjectError::BadTarget {
            target: bad.target.clone(),
        });
    }
    if binds.is_empty() {
        return Ok(());
    }
    let prepared = binds
        .iter()
        .map(|bind| prepare(calls, bind))
        .collect::<Result<Vec<_>, _>>()?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| enter_and_attach(calls, pid, &prepared, ids))
            .join()
            .unwrap_or(Err(InjectError::Panicked { pid }))
    })
}

/// Whether `target` is `/` and then one or more parts, none of them empty, `.` or `..`.
fn normalized(target: &str) -> bool {
    target
        .strip_prefix('/')
        .is_some_and(|rest| rest.split('/').all(|part| !matches!(part, "" | "." | "..")))
}

/// Opens, clones and sets the source of `bind`, in the daemon's namespace.
fn prepare<'a>(calls: &dyn Inject, bind: &'a HostMount) -> Result<Prepared<'a>, InjectError> {
    let source = calls
        .open_source(&bind.source)
        .map_err(failed(InjectStep::OpenSource, bind))?;
    let is_dir = calls
        .is_dir(source.as_fd())
        .map_err(failed(InjectStep::Inspect, bind))?;
    let tree = calls
        .clone_tree(source.as_fd())
        .map_err(failed(InjectStep::Clone, bind))?;
    let attributes = Attributes {
        read_only: bind.read_only,
        no_exec: bind.no_exec,
    };
    calls
        .set_attributes(tree.as_fd(), attributes)
        .map_err(failed(InjectStep::Attributes, bind))?;
    Ok(Prepared { bind, tree, is_dir })
}

/// What the thread does: enters the container, then makes the mount point of each of
/// `prepared` and attaches it.
fn enter_and_attach(
    calls: &dyn Inject,
    pid: u32,
    prepared: &[Prepared<'_>],
    ids: (u32, u32),
) -> Result<(), InjectError> {
    let namespace = calls
        .open_namespace(pid)
        .map_err(entering(pid, "open namespace"))?;
    let root = calls.open_root(pid).map_err(entering(pid, "open root"))?;
    calls.unshare_fs().map_err(entering(pid, "unshare fs"))?;
    calls
        .join(namespace.as_fd())
        .map_err(entering(pid, "join"))?;
    calls
        .change_dir(root.as_fd())
        .map_err(entering(pid, "change dir"))?;
    calls.change_root().map_err(entering(pid, "change root"))?;
    prepared
        .iter()
        .try_for_each(|one| attach_one(calls, one, ids))
}

/// Makes the mount point of `one` as the job's user, and attaches its clone there as the
/// daemon's.
fn attach_one(calls: &dyn Inject, one: &Prepared<'_>, ids: (u32, u32)) -> Result<(), InjectError> {
    let bind = one.bind;
    calls
        .set_fs_ids(ids)
        .map_err(failed(InjectStep::BecomeJob, bind))?;
    make_mount_point(calls, bind, one.is_dir).map_err(failed(InjectStep::MakeMountPoint, bind))?;
    calls
        .set_fs_ids((0, 0))
        .map_err(failed(InjectStep::BecomeRoot, bind))?;
    calls
        .attach(one.tree.as_fd(), Path::new(&bind.target))
        .map_err(failed(InjectStep::Attach, bind))
}

/// Makes every directory of the target of `bind` from the first part down, and the target
/// itself: a directory when `is_dir`, else an empty file.
fn make_mount_point(calls: &dyn Inject, bind: &HostMount, is_dir: bool) -> io::Result<()> {
    let parts: Vec<&str> = bind.target.split('/').skip(1).collect();
    let mut path = PathBuf::from("/");
    for (at, part) in parts.iter().enumerate() {
        path.push(part);
        if at + 1 == parts.len() && !is_dir {
            calls.make_file(&path)?;
        } else {
            calls.make_dir(&path)?;
        }
    }
    Ok(())
}
