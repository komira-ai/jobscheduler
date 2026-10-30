//! The per-run scratch disk quota as the lifecycle uses it: what a run books when it names
//! no disk, and the calls on the quota around a run's scratch directory.
//!
//! The mechanism and the order of its calls are in [`crate::quota`]. This is the half that
//! knows about runs and slots: a slot's project id, the bytes a run is bounded to (the bytes
//! it books, so the agent and the server count the same number), and the inode limit.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use super::{Event, Finding, code};
use crate::quota::{Mode, ProjectId, ProjectIds, QuotaError, ScratchQuota, Usage};

/// How long a quota that failed `max_failures` times in a row is left alone before it is
/// probed again, and how often an unhealthy quota is reported again.
pub(super) const REPROBE_MS: u64 = 60_000;

/// How long a slot that stays parked is left alone before it is reported again.
pub(super) const REMIND_PARKED_MS: u64 = 3_600_000;

/// The quota of an agent started with `--scratch-quota` whose scratch root passed the probe.
#[derive(Clone)]
pub struct RunQuota {
    quota: Arc<dyn ScratchQuota>,
    ids: ProjectIds,
    default_disk_bytes: u64,
    max_inodes: u64,
    max_failures: u32,
}

impl fmt::Debug for RunQuota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunQuota")
            .field("ids", &self.ids)
            .field("default_disk_bytes", &self.default_disk_bytes)
            .field("max_inodes", &self.max_inodes)
            .field("max_failures", &self.max_failures)
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
            max_failures: super::DEFAULT_SCRATCH_QUOTA_FAILURES,
        }
    }

    /// The same quota that is probed again after `max_failures` failed calls in a row (at
    /// least 1).
    pub fn with_max_failures(self, max_failures: u32) -> Self {
        Self {
            max_failures: max_failures.max(1),
            ..self
        }
    }

    /// The failed calls in a row after which the quota is probed again.
    pub fn max_failures(&self) -> u32 {
        self.max_failures
    }

    /// Whether the scratch root can still enforce the quota, and why not.
    pub(super) fn probe(&self) -> crate::quota::Probe {
        self.quota.probe()
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

/// Whether the quota still works. A quota whose calls fail `max_failures` times in a row
/// (a mount that lost `prjquota`, a failing `apply`, `clear` or usage read) is probed
/// again; one the probe no longer calls `Project` is *unhealthy*: no run starts on it (it
/// is refused `no-capacity`, the one retriable code the wire has; no taint is written),
/// it is reported again every [`REPROBE_MS`], and it is probed again then, so a host that
/// recovers takes runs again without a restart.
#[derive(Default)]
pub(super) struct QuotaHealth {
    failures: u32,
    unhealthy: Option<String>,
    next_ms: u64,
}

impl QuotaHealth {
    /// A call worked: the failures in a row start again from none.
    pub(super) fn ok(&mut self) {
        self.failures = 0;
    }

    /// Why the quota is unhealthy, when it is.
    pub(super) fn why(&self) -> Option<&str> {
        self.unhealthy.as_deref()
    }

    /// A call failed. After `max_failures` in a row the quota is probed; if the probe is not
    /// `Project` the quota is unhealthy from now.
    pub(super) fn failed(&mut self, quota: &RunQuota, now_ms: u64, events: &mut Vec<Event>) {
        self.failures = self.failures.saturating_add(1);
        if self.failures < quota.max_failures() || self.unhealthy.is_some() {
            return;
        }
        let probe = quota.probe();
        match probe.mode {
            Mode::Project => self.failures = 0,
            Mode::None => {
                self.next_ms = now_ms.saturating_add(REPROBE_MS);
                events.push(Self::report(&probe.reason, self.failures));
                self.unhealthy = Some(probe.reason);
            }
        }
    }

    fn report(reason: &str, failures: u32) -> Event {
        Event::Finding(Finding {
            run_id: None,
            code: code::QUOTA_UNHEALTHY,
            message: format!(
                "the scratch quota is unhealthy after {failures} failed calls in a row \
                 ({reason}): no run starts here until it works again"
            ),
        })
    }

    /// Called at every step: an unhealthy quota is probed again and reported again once
    /// every [`REPROBE_MS`]; one that probes `Project` again is healthy.
    pub(super) fn tick(&mut self, quota: &RunQuota, now_ms: u64, events: &mut Vec<Event>) {
        if self.unhealthy.is_none() || now_ms < self.next_ms {
            return;
        }
        let probe = quota.probe();
        self.next_ms = now_ms.saturating_add(REPROBE_MS);
        match probe.mode {
            Mode::Project => {
                *self = Self::default();
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::QUOTA_RECOVERED,
                    message: "the scratch quota works again: runs start here".to_owned(),
                }));
            }
            Mode::None => {
                events.push(Self::report(&probe.reason, self.failures));
                self.unhealthy = Some(probe.reason);
            }
        }
    }
}
