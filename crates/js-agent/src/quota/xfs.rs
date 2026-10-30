//! The XFS project quota backend of [`ScratchQuota`]. See the module documentation of
//! [`super`] for the order a run uses it in.
//!
//! Nothing here is `unsafe`: the kernel calls are in `sys`, behind a trait the tests script.
//!
//! **Which device.** `quotactl` names the file system by a block device node. The node is
//! the source field of the scratch root's line in `/proc/self/mountinfo`. For a volume that
//! is a loop image the source is the loop device (`/dev/loopN`), which is the right
//! argument, because the XFS on it is mounted from that device and its quota is kept by it;
//! the backing file's file system is another one. The probe does not trust the name: the
//! node must be a block device whose device number is the one the mount reports, so a
//! source that is renamed (a container's `/dev`) or is not a device (a bind mount of a
//! file, an anonymous device) is refused instead of aimed at some other file system.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::mountinfo;
use super::sys::{Kernel, PROJECT_ACCT_AND_ENFD, Sys, XFLAG_PROJINHERIT};
use super::{
    Probe, ProjectId, ProjectIds, QuotaError, ScratchQuota, Usage, check_limits, io_err,
    is_child_of,
};

/// Project quota on the XFS file system that holds one scratch root.
pub struct XfsQuota {
    sys: Box<dyn Sys>,
    root: PathBuf,
    ids: ProjectIds,
    /// The device the probe (or the first call after it) found, so the mount table is not
    /// read again by every call: a restart reads the usage of thousands of slots. It is
    /// forgotten when a call that uses it fails, and found again by the next one.
    device: Mutex<Option<(PathBuf, u64)>>,
}

impl XfsQuota {
    /// The quota of the scratch root `root` (the canonical path, as `ScratchRoot::path`
    /// gives it); `ids` says which ids belong to the agent.
    pub fn new(root: PathBuf, ids: ProjectIds) -> Self {
        Self::with_sys(Box::new(Kernel), root, ids)
    }

    fn with_sys(sys: Box<dyn Sys>, root: PathBuf, ids: ProjectIds) -> Self {
        Self {
            sys,
            root,
            ids,
            device: Mutex::new(None),
        }
    }

    /// The limits the project has, in bytes and inodes; 0 is no limit. For tests and for an
    /// operator looking at a host.
    ///
    /// # Errors
    ///
    /// The root cannot carry a quota, or a system call failed. A project the file system
    /// does not know has no limits.
    pub fn limits(&self, project: ProjectId) -> Result<(u64, u64), QuotaError> {
        let device = self.device()?;
        let dq = self.quota_of(&device, project)?;
        Ok((dq.blocks_limit.saturating_mul(512), dq.inodes_limit))
    }

    /// The block device node of the root's file system and the device number (`st_dev`) the
    /// mount reports, if it is XFS; otherwise why not.
    fn find_device(&self) -> Result<(PathBuf, u64), String> {
        let root = self.root.display();
        let text = self
            .sys
            .mountinfo()
            .map_err(|why| format!("cannot read /proc/self/mountinfo: {why}"))?;
        let mount = mountinfo::find(&text, &self.root)
            .ok_or_else(|| format!("{root} is on no mount that /proc/self/mountinfo lists"))?;
        if mount.fstype != "xfs" {
            return Err(format!("{root} is on {}, not xfs", mount.fstype));
        }
        let source = Path::new(&mount.source);
        if !source.starts_with("/dev/") {
            return Err(format!(
                "the mount source {} is not a device node under /dev",
                mount.source
            ));
        }
        match self.sys.block_rdev(source) {
            Ok(Some(rdev)) if rdev == mount.dev => Ok((source.to_owned(), mount.dev)),
            Ok(Some(_)) => Err(format!(
                "{} is not the device the mount reports (a different major:minor)",
                mount.source
            )),
            Ok(None) => Err(format!("{} is not a block device", mount.source)),
            Err(why) => Err(format!("cannot look at {}: {why}", mount.source)),
        }
    }

    /// Refuses a directory that is not a direct child of the root, by its components.
    fn require_child(&self, run_dir: &Path) -> Result<(), QuotaError> {
        match is_child_of(&self.root, run_dir) {
            true => Ok(()),
            false => Err(QuotaError::Outside(run_dir.to_owned())),
        }
    }

    /// The device of the root and its number: the one remembered, or found from the mount
    /// table now.
    fn known(&self) -> Result<(PathBuf, u64), QuotaError> {
        let mut known = self.device.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(device) = known.as_ref() {
            return Ok(device.clone());
        }
        let found = self.find_device().map_err(QuotaError::Unsupported)?;
        *known = Some(found.clone());
        Ok(found)
    }

    /// The device of the root: the one remembered, or found from the mount table now.
    fn device(&self) -> Result<PathBuf, QuotaError> {
        self.known().map(|(device, _)| device)
    }

    /// The device for an `apply`: the one remembered, unless the root is on another file
    /// system than it names (a volume that was swapped under the same path): then it is
    /// forgotten and found again. A cached node is otherwise trusted for every call, so the
    /// check is made where a run's limits are set, once for each run.
    fn device_of_the_root_now(&self) -> Result<PathBuf, QuotaError> {
        let (device, dev) = self.known()?;
        let root = io_err(self.sys.dev_of(&self.root), "look at the scratch root")?;
        if root == dev {
            return Ok(device);
        }
        self.forget_device();
        self.device()
    }

    /// Drops the remembered device, after a call that used it failed (the mount may have
    /// changed): the next call reads the mount table again.
    fn forget_device(&self) {
        *self.device.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The limits and usage of a project; a project the file system has no record of
    /// (`ENOENT`) has none.
    fn quota_of(&self, device: &Path, project: ProjectId) -> Result<super::sys::Dq, QuotaError> {
        match self.sys.get_quota(device, project.get()) {
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            other => {
                if other.is_err() {
                    self.forget_device();
                }
                io_err(other, "read the project quota")
            }
        }
    }

    /// Sets the project id and the inherit flag on the open directory `dir`, and reads them
    /// back from the same descriptor.
    fn assign(&self, dir: &File, project: ProjectId) -> Result<(), QuotaError> {
        let mut attr = io_err(
            self.sys.get_attr(dir),
            "read the project id of the directory",
        )?;
        attr.projid = project.get();
        attr.xflags |= XFLAG_PROJINHERIT;
        io_err(
            self.sys.set_attr(dir, attr),
            "set the project id of the directory",
        )?;
        let back = io_err(self.sys.get_attr(dir), "read back the project id")?;
        (back.projid == attr.projid && back.xflags & XFLAG_PROJINHERIT != 0)
            .then_some(())
            .ok_or(QuotaError::Lost { project })
    }

    /// The whole of the probe: why it fails, or the device that carries the quota.
    fn check(&self) -> Result<PathBuf, String> {
        // The probe always reads the mount table, and what it finds is what later calls use.
        let found = self.find_device();
        *self.device.lock().unwrap_or_else(PoisonError::into_inner) = found.as_ref().ok().cloned();
        let (device, _) = found?;
        let flags = self
            .sys
            .quota_flags(&device)
            .map_err(|why| format!("cannot read the quota state of {}: {why}", device.display()))?;
        if flags & PROJECT_ACCT_AND_ENFD != PROJECT_ACCT_AND_ENFD {
            return Err(format!(
                "{} is mounted without prjquota: project quota is not counted and enforced",
                device.display()
            ));
        }
        let dir = self.root.join(probe_name());
        fs::create_dir(&dir)
            .map_err(|why| format!("cannot make a probe directory in the scratch root: {why}"))?;
        let set = io_err(self.sys.open_dir(&dir), "open the probe directory")
            .and_then(|fd| self.assign(&fd, self.ids.probe_id()));
        // A directory left behind (the remove failed, or the process died) is never used
        // again (its name is random) and is removed by the scratch sweep when the agent
        // next adopts its runs (lifecycle/machine/adopt.rs, ScratchRoot::sweep).
        let _ = fs::remove_dir(&dir);
        set.map(|()| device)
            .map_err(|why| format!("project ids cannot be set: {why}"))
    }
}

/// The name of a probe directory: the process id and 64 random bits, so a directory that an
/// earlier process with the same id left behind (a crash; in containers the id is often the
/// same) is not the one this probe makes. Were the system's randomness unreadable the bits
/// are 0, and a leftover of that name makes the probe say none, which is safe.
fn probe_name() -> String {
    let mut bits = [0_u8; 8];
    let _ = getrandom::getrandom(&mut bits);
    format!(
        ".js-quota-probe-{}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(bits)
    )
}

impl ScratchQuota for XfsQuota {
    fn probe(&self) -> Probe {
        match self.check() {
            Ok(device) => Probe::project(format!("xfs project quota on {}", device.display())),
            Err(why) => Probe::none(why),
        }
    }

    fn apply(
        &self,
        run_dir: &Path,
        project: ProjectId,
        block_limit_bytes: u64,
        inode_limit: u64,
    ) -> Result<(), QuotaError> {
        let blocks = check_limits(block_limit_bytes, inode_limit)?;
        self.require_child(run_dir)?;
        let device = self.device_of_the_root_now()?;
        // One descriptor for everything: what is checked empty is what gets the project id.
        let dir = io_err(
            self.sys.open_dir(run_dir),
            "open the run's scratch directory",
        )?;
        let empty = io_err(self.sys.is_empty(&dir), "list the run's scratch directory")?;
        if !empty {
            return Err(QuotaError::NotEmpty(run_dir.to_owned()));
        }
        self.assign(&dir, project)?;
        let set = self
            .sys
            .set_limits(&device, project.get(), blocks, inode_limit);
        if set.is_err() {
            self.forget_device();
        }
        io_err(set, "set the limits of the project")
    }

    fn usage(&self, project: ProjectId) -> Result<Usage, QuotaError> {
        let device = self.device()?;
        let dq = self.quota_of(&device, project)?;
        Ok(Usage {
            bytes: dq.blocks_used.saturating_mul(512),
            inodes: dq.inodes_used,
        })
    }

    fn clear(&self, run_dir: &Path, project: ProjectId) -> Result<(), QuotaError> {
        self.require_child(run_dir)?;
        let device = self.device()?;
        // The limits go to 0 (none) with `Q_XSETQLIM`, not `Q_XQUOTARM`: that command frees
        // the quota files of the whole file system, every run's project, and only works
        // with quota accounting off. The project's record stays, with no limit and the
        // usage it still has, which is what `usage` reads before the id is reused.
        let off = self.sys.set_limits(&device, project.get(), 0, 0);
        if off.is_err() {
            self.forget_device();
        }
        io_err(off, "remove the limits of the project")?;
        let dir = match self.sys.open_dir(run_dir) {
            // The directory is gone, and the project id with it.
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => io_err(other, "open the run's scratch directory")?,
        };
        let mut attr = io_err(
            self.sys.get_attr(&dir),
            "read the project id of the directory",
        )?;
        attr.projid = 0;
        attr.xflags &= !XFLAG_PROJINHERIT;
        io_err(
            self.sys.set_attr(&dir, attr),
            "remove the project id from the directory",
        )
    }
}

#[cfg(test)]
mod tests;
