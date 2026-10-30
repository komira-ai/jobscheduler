//! Project ids: one per run slot, never handed out while the id still holds usage.

use std::fmt;

use super::{QuotaError, ScratchQuota};

/// How many slots have an id: the agent's job limit (`lifecycle::SLOTS`), rounded up to a
/// power of two.
pub const MAX_SLOTS: u32 = 4096;

/// A project id, always above 0 (0 is "no project") and at most `i32::MAX` (the id of
/// `quotactl` is a signed int).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectId(u32);

impl ProjectId {
    /// The number the kernel knows the project by.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The project ids of a host: the slot `n` is the id `base + n`. Two live runs hold two
/// slots, so they never share an id; a slot is given again only after its cleanup passed,
/// and [`ProjectIds::claim`] checks that the id reads zero usage before it is used again.
///
/// The range `base ..= base + MAX_SLOTS` (the last is the probe's) belongs to the agent:
/// nothing else on the host may use these ids on the scratch file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectIds {
    base: u32,
}

impl Default for ProjectIds {
    fn default() -> Self {
        Self {
            base: Self::DEFAULT_BASE,
        }
    }
}

impl ProjectIds {
    /// The default base, `0x6A73_0000`: far above the ids an operator numbers projects by.
    pub const DEFAULT_BASE: u32 = 0x6A73_0000;

    /// Ids from `base`.
    ///
    /// # Errors
    ///
    /// The base is 0, or the range does not fit below `i32::MAX`.
    pub fn new(base: u32) -> Result<Self, QuotaError> {
        let fits = base
            .checked_add(MAX_SLOTS)
            .is_some_and(|last| i32::try_from(last).is_ok());
        match (base, fits) {
            (0, _) => Err(QuotaError::BadProject("the base may not be 0")),
            (_, false) => Err(QuotaError::BadProject("the range does not fit in an i32")),
            (_, true) => Ok(Self { base }),
        }
    }

    /// The id of `slot`.
    ///
    /// # Errors
    ///
    /// The slot is `MAX_SLOTS` or more.
    pub fn of_slot(&self, slot: u32) -> Result<ProjectId, QuotaError> {
        match slot < MAX_SLOTS {
            true => Ok(ProjectId(self.base + slot)),
            false => Err(QuotaError::BadProject("the slot has no project id")),
        }
    }

    /// The id the probe sets on its test directory: the one after the last slot's.
    pub fn probe_id(&self) -> ProjectId {
        ProjectId(self.base + MAX_SLOTS)
    }

    /// The id of `slot`, if it holds no usage: an earlier run's files may still be counted
    /// against it, and a new run must not inherit them or their limit.
    ///
    /// # Errors
    ///
    /// The slot has no id, the usage cannot be read, or the id still holds usage
    /// ([`QuotaError::InUse`]).
    pub fn claim(&self, quota: &dyn ScratchQuota, slot: u32) -> Result<ProjectId, QuotaError> {
        let project = self.of_slot(slot)?;
        let usage = quota.usage(project)?;
        match usage.is_zero() {
            true => Ok(project),
            false => Err(QuotaError::InUse { project, usage }),
        }
    }
}
