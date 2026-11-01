//! The enforcement half of the per-run scratch disk quota: a mechanism behind
//! [`ScratchQuota`], and a probe that says whether this host can use it.
//!
//! With `--scratch-quota` the agent probes the scratch root's file system at start ([`system`],
//! [`ScratchQuota::probe`]) and refuses to start unless it is [`Mode::Project`]; the
//! lifecycle then uses the quota for every run (`lifecycle::RunQuota`). A run uses the quota
//! in this order, and each step is a method of the trait:
//!
//! 1. [`ProjectIds::claim`] a project id for the run's slot (refused while a run that held
//!    the id before has usage left on it);
//! 2. [`ScratchQuota::apply`] on the run's **empty** scratch directory, before it is
//!    chowned to the slot's user and before anything is written to it (a file created
//!    earlier carries project 0 and escapes the count: `apply` refuses a directory that is
//!    not empty);
//! 3. run the job; [`ScratchQuota::usage`] reads what it has used;
//! 4. remove the tree (`ScratchRoot::remove`), then [`ScratchQuota::clear`]. A `clear`
//!    that fails keeps the slot held and is tried again at the next step; a restart clears
//!    the projects of the directories its sweep removes, and a slot whose id still reads
//!    usage is parked until it reads zero.
//!
//! The mechanism is XFS project quota (`prjquota`), Linux only: the directory gets a
//! project id and `FS_XFLAG_PROJINHERIT`, so everything below it is counted against the
//! project, and the project has a block and an inode hard limit, so a writer is refused at
//! the limit. XFS reports a project quota hit as `ENOSPC` (the project looks like a full
//! file system), not `EDQUOT`, which user and group quotas give. Every other platform and
//! file system probes to [`Mode::None`].

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod fake;
mod ids;
#[cfg(target_os = "linux")]
mod mountinfo;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod xfs;

pub use fake::{Call, FakeQuota};
pub use ids::{MAX_SLOTS, ProjectId, ProjectIds};
#[cfg(target_os = "linux")]
pub use xfs::XfsQuota;

/// What [`probe`] found out about a scratch root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Project quota accounting is on for the file system and project ids can be set: a
    /// run's scratch can be bounded.
    Project,
    /// No quota: the platform or the file system cannot bound a directory.
    None,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Project => "project",
            Self::None => "none",
        })
    }
}

/// The answer of a probe: the mode, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Whether a run's scratch can be bounded.
    pub mode: Mode,
    /// What was found: the device that carries the quota, or what is missing.
    pub reason: String,
}

impl Probe {
    /// A probe that found project quota usable.
    pub fn project(reason: impl Into<String>) -> Self {
        Self {
            mode: Mode::Project,
            reason: reason.into(),
        }
    }

    /// A probe that found no usable quota, for `reason`.
    pub fn none(reason: impl Into<String>) -> Self {
        Self {
            mode: Mode::None,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.mode, self.reason)
    }
}

/// What a project has used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes of disk space, as the file system counts them.
    pub bytes: u64,
    /// Inodes (files, directories, links).
    pub inodes: u64,
}

impl Usage {
    /// Nothing used.
    pub const ZERO: Self = Self {
        bytes: 0,
        inodes: 0,
    };

    /// Whether nothing is used.
    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

/// Why a quota call failed.
#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    /// The scratch root cannot carry a quota (the probe said none): the text is its reason.
    #[error("no project quota: {0}")]
    Unsupported(String),
    /// A limit that would mean "no limit" or is not usable.
    #[error("bad limit: {0}")]
    BadLimit(&'static str),
    /// A slot or a base that has no project id.
    #[error("bad project id: {0}")]
    BadProject(&'static str),
    /// The directory already holds something: its contents would not be counted.
    #[error("{} is not empty: files made before the project id is set escape the quota", .0.display())]
    NotEmpty(PathBuf),
    /// The directory is not a direct child of the scratch root the quota was made for.
    #[error("{} is not a directory directly below the scratch root", .0.display())]
    Outside(PathBuf),
    /// The project still has usage from an earlier run: its id is not reused yet.
    #[error("project {project} still holds {} bytes in {} inodes", .usage.bytes, .usage.inodes)]
    InUse {
        /// The id that was asked for.
        project: ProjectId,
        /// What it still holds.
        usage: Usage,
    },
    /// The file system accepted the project id and does not show it.
    #[error("the file system did not keep project {project}")]
    Lost {
        /// The id that was set.
        project: ProjectId,
    },
    /// A system call failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The system's reason.
        source: io::Error,
    },
}

impl From<QuotaError> for io::Error {
    fn from(err: QuotaError) -> Self {
        Self::other(err)
    }
}

/// Wraps the error of a system call with what was being done.
pub(crate) fn io_err<T>(result: io::Result<T>, what: &'static str) -> Result<T, QuotaError> {
    result.map_err(|source| QuotaError::Io { what, source })
}

/// The largest block limit an `apply` takes, in bytes: the bound of a job's disk limit on
/// the wire ([`js_agent_wire::limits::MAX_DISK_BYTES`], 2^46 - 1). Blocks of 512 bytes
/// then come to at most 2^37, and the kernel's shift back up to bytes cannot wrap to 0,
/// which would mean no limit.
pub const MAX_BLOCK_LIMIT_BYTES: u64 = js_agent_wire::limits::MAX_DISK_BYTES;

/// The largest inode limit an `apply` takes: what a 32-bit inode count holds, far above
/// what one run's scratch makes.
pub const MAX_INODE_LIMIT: u64 = u32::MAX as u64;

/// Checks the limits of an `apply` and returns the block limit in 512-byte blocks, the unit
/// of the kernel interface (a size is rounded up). A limit of 0 is refused: for the kernel it
/// means no limit at all. So is one above [`MAX_BLOCK_LIMIT_BYTES`] or [`MAX_INODE_LIMIT`].
pub(crate) fn check_limits(block_limit_bytes: u64, inode_limit: u64) -> Result<u64, QuotaError> {
    match (block_limit_bytes, inode_limit) {
        (0, _) => Err(QuotaError::BadLimit("a block limit of 0 means no limit")),
        (_, 0) => Err(QuotaError::BadLimit("an inode limit of 0 means no limit")),
        (bytes, _) if bytes > MAX_BLOCK_LIMIT_BYTES => {
            Err(QuotaError::BadLimit("a block limit above 2^46 - 1 bytes"))
        }
        (_, inodes) if inodes > MAX_INODE_LIMIT => {
            Err(QuotaError::BadLimit("an inode limit above 2^32 - 1"))
        }
        (bytes, _) => Ok(bytes.div_ceil(512)),
    }
}

/// Whether `run_dir` is a directory directly below `root`: its parent is the root itself
/// and its last component is a name. The test is on the components, with no look at the
/// file system: a path that goes up (`root/../x`, `root/..`) or is the root itself is not a
/// child, and a deeper path is not a run's directory.
#[cfg(target_os = "linux")]
pub(crate) fn is_child_of(root: &Path, run_dir: &Path) -> bool {
    run_dir.parent() == Some(root) && run_dir.file_name().is_some()
}

/// Refuses a directory that holds anything (see [`QuotaError::NotEmpty`]).
pub(crate) fn require_empty(dir: &Path) -> Result<(), QuotaError> {
    let mut entries = io_err(fs::read_dir(dir), "list the run's scratch directory")?;
    match entries.next() {
        None => Ok(()),
        Some(_) => Err(QuotaError::NotEmpty(dir.to_owned())),
    }
}

/// A way to bound the disk a run's scratch directory may use.
pub trait ScratchQuota: Send + Sync {
    /// Whether this quota can work on the scratch root it was made for, and why.
    fn probe(&self) -> Probe;

    /// Bounds the **empty** directory `run_dir` (a direct child of the scratch root) as project
    /// `project`: everything made below it is counted against the project and is refused
    /// (`ENOSPC` on XFS project quota) at `block_limit_bytes` of space or `inode_limit`
    /// inodes. Call it before the directory is chowned or written to. A failed call may leave
    /// the project id on the directory; the run is not started, and the directory is removed.
    ///
    /// # Errors
    ///
    /// A limit of 0, a directory that is not empty or is not a direct child of the scratch root, a
    /// root that cannot carry a quota, or a failed system call.
    fn apply(
        &self,
        run_dir: &Path,
        project: ProjectId,
        block_limit_bytes: u64,
        inode_limit: u64,
    ) -> Result<(), QuotaError>;

    /// What the project holds now. A project the file system does not know holds nothing.
    ///
    /// # Errors
    ///
    /// The root cannot carry a quota, or a system call failed.
    fn usage(&self, project: ProjectId) -> Result<Usage, QuotaError>;

    /// Takes the bound off: the limits of the project go to 0 (none), then the project id
    /// is removed from `run_dir`, unless the directory is already gone (the id went with
    /// it). Call it after the tree is removed. A failure means the project is not clean:
    /// the caller keeps the slot unreleased, as it does for a failed removal, and may try
    /// again.
    ///
    /// # Errors
    ///
    /// `run_dir` is not a direct child of the scratch root, the root cannot carry a quota,
    /// or a system call failed.
    fn clear(&self, run_dir: &Path, project: ProjectId) -> Result<(), QuotaError>;
}

/// The project id of the open directory `dir`, or `None` where its file system keeps no
/// project ids (the kernel does not know the call). Any other failure is an error: a read
/// that failed says nothing about the directory's project.
///
/// # Errors
///
/// The system's reason, when the read failed for another reason than the file system having
/// no project ids.
#[cfg(target_os = "linux")]
pub fn project_of(dir: &std::fs::File) -> io::Result<Option<u32>> {
    use sys::{Kernel, Sys};
    projid_of(Kernel.get_attr(dir))
}

/// What a read of a directory's attributes says of its project id (see [`project_of`]).
#[cfg(target_os = "linux")]
fn projid_of(read: io::Result<sys::Fsx>) -> io::Result<Option<u32>> {
    match read {
        Ok(attr) => Ok(Some(attr.projid)),
        // FOR_A_RED_RUN: the old, fail-open reading.
        Err(_) => Ok(Some(0)),
    }
}

/// The quota of the scratch root `scratch_root` on this machine: XFS project quota with the
/// agent's project ids. On a platform without one, a quota that probes to [`Mode::None`].
#[cfg(target_os = "linux")]
pub fn system(scratch_root: &Path) -> Arc<dyn ScratchQuota> {
    Arc::new(XfsQuota::new(
        scratch_root.to_owned(),
        ProjectIds::default(),
    ))
}

/// The quota of the scratch root on this machine: on this platform, one that probes to
/// [`Mode::None`] and refuses every call.
#[cfg(not(target_os = "linux"))]
pub fn system(_scratch_root: &Path) -> Arc<dyn ScratchQuota> {
    Arc::new(FakeQuota::unsupported("no project quota on this platform"))
}

/// What the file system of `scratch_root` offers: [`Mode::Project`] for XFS mounted with
/// `prjquota` where project ids can be set, otherwise [`Mode::None`] with the reason.
/// On a root that is on XFS this makes a directory in the root and removes it again.
#[cfg(target_os = "linux")]
pub fn probe(scratch_root: &Path) -> Probe {
    XfsQuota::new(scratch_root.to_owned(), ProjectIds::default()).probe()
}

/// What the file system of `scratch_root` offers: on this platform, never a quota.
#[cfg(not(target_os = "linux"))]
pub fn probe(_scratch_root: &Path) -> Probe {
    Probe::none("no project quota on this platform")
}

#[cfg(all(test, target_os = "linux"))]
mod project_tests {
    use std::io;

    use super::projid_of;
    use super::sys::Fsx;

    /// Catches a project id read that fails open: a read that failed for a reason of its own is
    /// an error (it says nothing of the directory, and two failed reads must never compare
    /// equal), a file system that does not know the call is the explicit `None`, and a read
    /// that worked is the id it gave.
    #[test]
    fn a_failed_read_is_an_error_and_only_an_unknown_call_is_no_project() {
        let attr = Fsx {
            projid: 7,
            ..Fsx::default()
        };
        assert_eq!(projid_of(Ok(attr)).unwrap(), Some(7));
        for errno in [libc::ENOTTY, libc::EOPNOTSUPP, libc::ENOSYS] {
            let read = Err(io::Error::from_raw_os_error(errno));
            assert_eq!(projid_of(read).unwrap(), None, "errno {errno}");
        }
        for errno in [libc::EIO, libc::EACCES, libc::EBADF] {
            let read = Err(io::Error::from_raw_os_error(errno));
            let why = projid_of(read).expect_err("a failed read is an error");
            assert_eq!(why.raw_os_error(), Some(errno));
        }
    }
}
