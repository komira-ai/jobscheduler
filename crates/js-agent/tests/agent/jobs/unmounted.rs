//! The mounts' calls of a test node's crun runtime, which mounts nothing.

use std::path::Path;

/// The mounts' calls of a node whose runtime mounts nothing: the directory of the runs'
/// mounts is a mount of its own already, and private; no other call is made.
#[derive(Debug)]
pub(crate) struct Unmounted;

impl js_contain::job_fs::MountCalls for Unmounted {
    fn clone_tree(&self, _: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn set_attributes(
        &self,
        _: std::os::fd::BorrowedFd<'_>,
        _: js_contain::job_fs::Attributes,
    ) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn attach(&self, _: std::os::fd::BorrowedFd<'_>, _: &Path) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn mount_overlay(&self, _: &Path, _: bool, _: &str) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn detach(&self, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::EINVAL))
    }
    fn is_mount_point(&self, _: &Path) -> std::io::Result<bool> {
        Ok(true)
    }
    fn bind_onto_itself(&self, _: &Path) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn make_private(&self, _: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

/// The mounts of a node's runtime opened from a plan: its directory `<node>/mounts`, which
/// is a private mount already, and nothing mounted.
pub(crate) fn seam(node: &Path) -> js_agent::crun::MountSeam {
    js_agent::crun::MountSeam {
        dir: node.join("mounts"),
        calls: std::sync::Arc::new(Unmounted),
        ..js_agent::crun::MountSeam::default()
    }
}
