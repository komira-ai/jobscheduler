//! [`Kernel`]: the system calls of [`MountCalls`], each made once with nothing around it,
//! so that every decision is in the code built from them (`super`), which a fake tests.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::CWD;
use rustix::mount::{
    MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};

use super::{Attributes, MountCalls};

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
