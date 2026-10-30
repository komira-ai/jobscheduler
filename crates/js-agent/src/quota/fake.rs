//! An in-memory [`ScratchQuota`] for tests of whatever uses one: it keeps the limits it is
//! given, the usage a test charges, and the calls in order, and it refuses what the real one
//! refuses (a limit of 0, a directory that is not empty).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Probe, ProjectId, QuotaError, ScratchQuota, Usage, check_limits, require_empty};

/// One call a [`FakeQuota`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `apply`, with its arguments.
    Apply {
        /// The run's scratch directory.
        dir: PathBuf,
        /// The project.
        project: ProjectId,
        /// The block limit in bytes.
        block_limit_bytes: u64,
        /// The inode limit.
        inode_limit: u64,
    },
    /// `usage` of a project.
    Usage(ProjectId),
    /// `clear`, with its arguments.
    Clear {
        /// The run's scratch directory.
        dir: PathBuf,
        /// The project.
        project: ProjectId,
    },
}

#[derive(Default)]
struct State {
    unsupported: Option<String>,
    calls: Vec<Call>,
    limits: BTreeMap<ProjectId, (u64, u64)>,
    used: BTreeMap<ProjectId, Usage>,
    fail_apply: bool,
    fail_clear: bool,
}

/// A [`ScratchQuota`] that keeps everything in memory.
#[derive(Default)]
pub struct FakeQuota {
    state: Mutex<State>,
}

impl FakeQuota {
    /// A quota that works.
    pub fn new() -> Self {
        Self::default()
    }

    /// A quota whose probe is none for `reason`, and whose calls are refused.
    pub fn unsupported(reason: &str) -> Self {
        let this = Self::default();
        this.state().unsupported = Some(reason.to_owned());
        this
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the next `apply` calls fail until it is called again with `false`.
    pub fn set_fail_apply(&self, fail: bool) {
        self.state().fail_apply = fail;
    }

    /// Makes `clear` calls fail until it is called again with `false`. A failed `clear`
    /// changes nothing, so a retry finds the project as it was.
    pub fn set_fail_clear(&self, fail: bool) {
        self.state().fail_clear = fail;
    }

    /// Records that the project holds `usage` (what a running job would have written).
    pub fn charge(&self, project: ProjectId, usage: Usage) {
        self.state().used.insert(project, usage);
    }

    /// The calls answered so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// The (block bytes, inodes) limits the project has now, if any.
    pub fn limits_of(&self, project: ProjectId) -> Option<(u64, u64)> {
        self.state().limits.get(&project).copied()
    }

    /// The state, or the refusal of an unsupported fake.
    fn working(&self) -> Result<MutexGuard<'_, State>, QuotaError> {
        let state = self.state();
        match &state.unsupported {
            Some(reason) => Err(QuotaError::Unsupported(reason.clone())),
            None => Ok(state),
        }
    }
}

fn scripted(what: &'static str) -> QuotaError {
    QuotaError::Io {
        what,
        source: io::Error::other("scripted failure"),
    }
}

impl ScratchQuota for FakeQuota {
    fn probe(&self) -> Probe {
        match &self.state().unsupported {
            Some(reason) => Probe::none(reason.clone()),
            None => Probe::project("fake"),
        }
    }

    fn apply(
        &self,
        run_dir: &Path,
        project: ProjectId,
        block_limit_bytes: u64,
        inode_limit: u64,
    ) -> Result<(), QuotaError> {
        check_limits(block_limit_bytes, inode_limit)?;
        let mut state = self.working()?;
        state.calls.push(Call::Apply {
            dir: run_dir.to_owned(),
            project,
            block_limit_bytes,
            inode_limit,
        });
        require_empty(run_dir)?;
        if state.fail_apply {
            return Err(scripted("apply"));
        }
        state
            .limits
            .insert(project, (block_limit_bytes, inode_limit));
        Ok(())
    }

    fn usage(&self, project: ProjectId) -> Result<Usage, QuotaError> {
        let mut state = self.working()?;
        state.calls.push(Call::Usage(project));
        Ok(state.used.get(&project).copied().unwrap_or_default())
    }

    fn clear(&self, run_dir: &Path, project: ProjectId) -> Result<(), QuotaError> {
        let mut state = self.working()?;
        state.calls.push(Call::Clear {
            dir: run_dir.to_owned(),
            project,
        });
        if state.fail_clear {
            return Err(scripted("clear"));
        }
        state.limits.remove(&project);
        Ok(())
    }
}
