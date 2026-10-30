//! The per-run scratch disk quota as the lifecycle uses it: what a run books when it names
//! no disk, and the calls on the quota around a run's scratch directory.
//!
//! The mechanism and the order of its calls are in [`crate::quota`]. This is the half that
//! knows about runs and slots: a slot's project id, the bytes a run is bounded to (the bytes
//! it books, so the agent and the server count the same number), and the inode limit.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::quota::{ProjectId, ProjectIds, QuotaError, ScratchQuota, Usage};

/// The quota of an agent started with `--scratch-quota` whose scratch root passed the probe.
#[derive(Clone)]
pub struct RunQuota {
    quota: Arc<dyn ScratchQuota>,
    ids: ProjectIds,
    default_disk_bytes: u64,
    max_inodes: u64,
}

impl fmt::Debug for RunQuota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunQuota")
            .field("ids", &self.ids)
            .field("default_disk_bytes", &self.default_disk_bytes)
            .field("max_inodes", &self.max_inodes)
            .finish_non_exhaustive()
    }
}

impl RunQuota {
    /// A quota that bounds a run to the bytes it books and to `max_inodes`, and books
    /// `default_disk_bytes` for a run that names no disk (the bytes of
    /// `scratch-default-gib`). The limits are checked by each `apply`.
    pub fn new(quota: Arc<dyn ScratchQuota>, default_disk_bytes: u64, max_inodes: u64) -> Self {
        Self {
            quota,
            ids: ProjectIds::default(),
            default_disk_bytes,
            max_inodes,
        }
    }

    /// The bytes a run without `disk_bytes` books.
    pub fn default_disk_bytes(&self) -> u64 {
        self.default_disk_bytes
    }

    /// The inode limit of each run.
    pub fn max_inodes(&self) -> u64 {
        self.max_inodes
    }

    /// The id of `slot`, if it holds no usage (see [`ProjectIds::claim`]).
    pub(super) fn claim(&self, slot: u32) -> Result<ProjectId, QuotaError> {
        self.ids.claim(&*self.quota, slot)
    }

    /// Bounds the empty directory `dir` of the run in `slot` to `bytes` and the inode limit.
    pub(super) fn apply(&self, dir: &Path, slot: u32, bytes: u64) -> Result<(), QuotaError> {
        let project = self.ids.of_slot(slot)?;
        self.quota.apply(dir, project, bytes, self.max_inodes)
    }

    /// Takes the bound of `slot`'s project off, after the tree `dir` is removed.
    pub(super) fn clear(&self, dir: &Path, slot: u32) -> Result<(), QuotaError> {
        self.quota.clear(dir, self.ids.of_slot(slot)?)
    }

    /// What `slot`'s project holds now.
    pub(super) fn usage(&self, slot: u32) -> Result<Usage, QuotaError> {
        self.quota.usage(self.ids.of_slot(slot)?)
    }
}
