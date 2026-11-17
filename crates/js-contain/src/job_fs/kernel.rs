//! [`Kernel`]: the system calls of [`MountCalls`], each made once with nothing around it,
//! so that every decision is in the code built from them (`super`), which a fake tests.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{CWD, FileType, Mode, OFlags, ResolveFlags};
use rustix::mount::{
    MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};
use rustix::thread::{LinkNameSpaceType, UnshareFlags};

use super::{Attributes, EnterCalls, MountCalls};

/// The system calls themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct Kernel;

impl MountCalls for Kernel {
    fn clone_tree(&self, source: BorrowedFd<'_>) -> io::Result<OwnedFd> {
        let flags = OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_RECURSIVE
            | OpenTreeFlags::AT_EMPTY_PATH;
        rustix::mount::open_tree(source, "", flags).map_err(io::Error::from)
    }

    fn set_attributes(&self, tree: BorrowedFd<'_>, attributes: Attributes) -> io::Result<()> {
        let flags = libc::AT_EMPTY_PATH | libc::AT_RECURSIVE;
        mount_setattr(tree, flags, &mount_attr(attributes))
    }

    fn attach(&self, tree: BorrowedFd<'_>, point: &Path) -> io::Result<()> {
        let flags = MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH;
        rustix::mount::move_mount(tree, "", CWD, point, flags).map_err(io::Error::from)
    }

    fn mount_overlay(&self, point: &Path, no_exec: bool, options: &str) -> io::Result<()> {
        let options = std::ffi::CString::new(options)?;
        rustix::mount::mount(
            "overlay",
            point,
            "overlay",
            overlay_flags(no_exec),
            &*options,
        )
        .map_err(io::Error::from)
    }

    fn detach(&self, point: &Path) -> io::Result<()> {
        let flags = UnmountFlags::DETACH | UnmountFlags::NOFOLLOW;
        rustix::mount::unmount(point, flags).map_err(io::Error::from)
    }

    fn is_mount_point(&self, dir: &Path) -> io::Result<bool> {
        let parent = dir.join("..");
        Ok(crate::walk::mount_id_at(CWD, dir)? != crate::walk::mount_id_at(CWD, &parent)?)
    }

    fn bind_onto_itself(&self, dir: &Path) -> io::Result<()> {
        rustix::mount::mount_bind_recursive(dir, dir).map_err(io::Error::from)
    }

    fn make_private(&self, dir: &Path) -> io::Result<()> {
        let flags = MountPropagationFlags::PRIVATE | MountPropagationFlags::REC;
        rustix::mount::mount_change(dir, flags).map_err(io::Error::from)
    }
}

impl EnterCalls for Kernel {
    fn open_source(&self, path: &Path) -> io::Result<OwnedFd> {
        let flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        rustix::fs::open(path, flags, Mode::empty()).map_err(io::Error::from)
    }

    fn is_dir(&self, source: BorrowedFd<'_>) -> io::Result<bool> {
        rustix::fs::fstat(source)
            .map(|stat| FileType::from_raw_mode(stat.st_mode).is_dir())
            .map_err(io::Error::from)
    }

    fn open_namespace(&self, pid: u32) -> io::Result<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::CLOEXEC;
        rustix::fs::open(format!("/proc/{pid}/ns/mnt"), flags, Mode::empty())
            .map_err(io::Error::from)
    }

    fn open_root(&self, pid: u32) -> io::Result<OwnedFd> {
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        rustix::fs::open(format!("/proc/{pid}/root"), flags, Mode::empty()).map_err(io::Error::from)
    }

    fn unshare_fs(&self) -> io::Result<()> {
        // SAFETY: `CLONE_FS` only gives this thread its own copy of the root, the working
        // directory and the umask, which no memory of this process refers to; the thread
        // then changes them without touching any other thread's.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS) }.map_err(io::Error::from)
    }

    fn join(&self, namespace: BorrowedFd<'_>) -> io::Result<()> {
        rustix::thread::move_into_link_name_space(namespace, Some(LinkNameSpaceType::Mount))
            .map_err(io::Error::from)
    }

    fn change_dir(&self, dir: BorrowedFd<'_>) -> io::Result<()> {
        rustix::process::fchdir(dir).map_err(io::Error::from)
    }

    fn change_root(&self) -> io::Result<()> {
        rustix::process::chroot(".").map_err(io::Error::from)
    }

    fn set_fs_ids(&self, ids: (u32, u32)) -> io::Result<()> {
        set_fs_ids(ids, set_fs_gid, set_fs_uid)
    }

    fn make_dir(&self, path: &Path) -> io::Result<()> {
        match rustix::fs::mkdir(path, Mode::from_raw_mode(0o755)) {
            Err(rustix::io::Errno::EXIST) => Ok(()),
            other => other.map_err(io::Error::from),
        }
    }

    fn make_file(&self, path: &Path) -> io::Result<()> {
        let flags = OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        rustix::fs::open(path, flags, Mode::from_raw_mode(0o644))
            .map(drop)
            .map_err(io::Error::from)
    }
}

/// Sets the calling thread's file system group id, then user id, to `ids` (user, group),
/// with the two calls `set_gid` and `set_uid`, each of which answers the id it replaced and
/// changes nothing when it is refused. Each is called a second time with the same id: that
/// answers the id now in force, which is the one asked for only if the first call took.
pub(super) fn set_fs_ids(
    ids: (u32, u32),
    set_gid: impl Fn(u32) -> libc::c_int,
    set_uid: impl Fn(u32) -> libc::c_int,
) -> io::Result<()> {
    let (uid, gid) = ids;
    let took = |set: &dyn Fn(u32) -> libc::c_int, id: u32| {
        set(id);
        u32::try_from(set(id)) == Ok(id)
    };
    if took(&set_gid, gid) && took(&set_uid, uid) {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }
}

/// `setfsgid(gid)` for the calling thread: the group id it had.
fn set_fs_gid(gid: u32) -> libc::c_int {
    // SAFETY: the call takes an integer and touches no memory; it changes only the calling
    // thread's file system group id.
    unsafe { libc::setfsgid(gid) }
}

/// `setfsuid(uid)` for the calling thread: the user id it had.
fn set_fs_uid(uid: u32) -> libc::c_int {
    // SAFETY: the call takes an integer and touches no memory; it changes only the calling
    // thread's file system user id.
    unsafe { libc::setfsuid(uid) }
}

/// The system calls of a job's file system, each made once with arguments that change
/// nothing, and the kernel's answer: `openat2` and `open_tree` open `/` for its name
/// alone (the descriptor is closed at once), and `move_mount` and `mount_setattr` name an
/// empty path without `AT_EMPTY_PATH`, which every kernel that has them refuses
/// (`ENOENT`, or `EPERM` without the privilege to mount). A kernel that lacks a call
/// answers it `ENOSYS`; any other answer says the call is there.
pub fn probe() -> [(&'static str, io::Result<()>); 4] {
    let path = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let attr = mount_attr(Attributes::default());
    [
        (
            "openat2",
            rustix::fs::openat2(CWD, "/", path, Mode::empty(), ResolveFlags::empty())
                .map(drop)
                .map_err(io::Error::from),
        ),
        (
            "open_tree",
            rustix::mount::open_tree(CWD, "/", OpenTreeFlags::OPEN_TREE_CLOEXEC)
                .map(drop)
                .map_err(io::Error::from),
        ),
        (
            "move_mount",
            rustix::mount::move_mount(CWD, "", CWD, "", MoveMountFlags::empty())
                .map_err(io::Error::from),
        ),
        ("mount_setattr", mount_setattr(CWD, 0, &attr)),
    ]
}

/// The flags of an overlay's mount.
pub(super) fn overlay_flags(no_exec: bool) -> MountFlags {
    let flags = MountFlags::NOSUID | MountFlags::NODEV;
    if no_exec {
        flags | MountFlags::NOEXEC
    } else {
        flags
    }
}

/// What [`MountCalls::set_attributes`] asks of the kernel for `attributes`: private, and
/// `nosuid` and `nodev` with read-only and `noexec` as asked. Nothing is cleared: a source
/// that is read-only on the host stays so in its clone.
pub(super) fn mount_attr(attributes: Attributes) -> libc::mount_attr {
    let mut set = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV;
    if attributes.read_only {
        set |= libc::MOUNT_ATTR_RDONLY;
    }
    if attributes.no_exec {
        set |= libc::MOUNT_ATTR_NOEXEC;
    }
    libc::mount_attr {
        attr_set: set,
        attr_clr: 0,
        propagation: libc::MS_PRIVATE,
        userns_fd: 0,
    }
}

/// `mount_setattr(tree, "", flags, attr, sizeof attr)`, which rustix does not wrap.
pub(super) fn mount_setattr(
    tree: BorrowedFd<'_>,
    flags: libc::c_int,
    attr: &libc::mount_attr,
) -> io::Result<()> {
    let size = std::mem::size_of::<libc::mount_attr>();
    // SAFETY: the system call reads `size` bytes at `attr`, a live `mount_attr` of exactly
    // that size borrowed for the call, and the NUL-terminated empty path, a static; it
    // writes no memory of this process. `tree` is a descriptor borrowed for the call.
    // FFI-BOUNDARY: both pointers are this process's and stay owned by it; the kernel keeps
    // neither after it returns.
    let answer = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            flags,
            std::ptr::from_ref(attr),
            size,
        )
    };
    checked(answer)
}

/// A raw system call's answer: 0 is success, anything else the error in `errno`.
pub(super) fn checked(answer: libc::c_long) -> io::Result<()> {
    if answer == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
