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
    usage_wait_ms: u64,
    abandon_limit: u32,
}

impl fmt::Debug for RunQuota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunQuota")
            .field("ids", &self.ids)
            .field("default_disk_bytes", &self.default_disk_bytes)
            .field("max_inodes", &self.max_inodes)
            .field("max_failures", &self.max_failures)
            .field("usage_wait_ms", &self.usage_wait_ms)
            .field("abandon_limit", &self.abandon_limit)
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
            usage_wait_ms: super::DEFAULT_SCRATCH_USAGE_WAIT_SECS * 1000,
            abandon_limit: super::DEFAULT_SCRATCH_ABANDON_LIMIT,
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

    /// The same quota that gives up waiting for a finished run's usage to read zero after
    /// `ms` milliseconds (at least 1).
    pub fn with_usage_wait_ms(self, ms: u64) -> Self {
        Self {
            usage_wait_ms: ms.max(1),
            ..self
        }
    }

    /// The same quota that is unavailable until the agent is restarted once `limit` slots
    /// were given up on (at least 1).
    pub fn with_abandon_limit(self, limit: u32) -> Self {
        Self {
            abandon_limit: limit.max(1),
            ..self
        }
    }

    /// The slots given up on, over the life of the agent, after which the quota is
    /// unavailable.
    pub fn abandon_limit(&self) -> u32 {
        self.abandon_limit
    }

    /// How long a finished run's slot is held for its usage to read zero.
    pub fn usage_wait_ms(&self) -> u64 {
        self.usage_wait_ms
    }

    /// The failed calls in a row after which the quota is probed again.
    pub fn max_failures(&self) -> u32 {
        self.max_failures
    }

    /// Whether the quota works: the probe says project, and a bound can be set on a
    /// directory of its own (`dir`, made and removed here), its usage read, and the bound
    /// taken off, with the id the probe uses. The probe alone can say project while a call
    /// keeps failing (a limit the kernel refuses), so what failed is exercised too.
    pub(super) fn check(&self, dir: &Path) -> Result<(), String> {
        let probe = self.quota.probe();
        if probe.mode != Mode::Project {
            return Err(probe.reason);
        }
        let project = self.ids.probe_id();
        let _ = std::fs::remove_dir(dir);
        std::fs::create_dir(dir)
            .map_err(|why| format!("cannot make the trial directory: {why}"))?;
        let tried = self
            .quota
            .apply(dir, project, 1 << 20, 16)
            .and_then(|()| self.quota.usage(project).map(|_| ()));
        let removed = std::fs::remove_dir(dir);
        let cleared = self.quota.clear(dir, project);
        let problems = [
            problem("a trial bound failed", tried),
            problem("the trial directory stays", removed),
            problem("clearing the trial bound failed", cleared),
        ];
        problems.into_iter().flatten().next().map_or(Ok(()), Err)
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

/// What went wrong in `result`, said as `what`.
fn problem(what: &str, result: Result<(), impl fmt::Display>) -> Option<String> {
    result.err().map(|why| format!("{what}: {why}"))
}

/// Whether the quota still works. A quota whose calls fail `max_failures` times in a row
/// (a mount that lost `prjquota`, a failing `apply`, `clear` or usage read) is checked
/// again ([`RunQuota::check`]: the probe, and a trial bound on a directory of its own); one
/// that fails the check is *unhealthy*: no run starts on it (each start is refused
/// `quota-unavailable`, which says the host's quota is unavailable and taints nothing),
/// it is reported again every [`REPROBE_MS`], and it is probed again then, so a host that
/// recovers takes runs again without a restart.
#[derive(Default)]
pub(super) struct QuotaHealth {
    failures: u32,
    unhealthy: Option<String>,
    next_ms: u64,
    /// The slots given up on since the agent started. A good run does not take one back.
    abandoned: u32,
    /// Set when `abandoned` reached the limit; only a restart of the agent clears it.
    locked: Option<String>,
}

impl QuotaHealth {
    /// A call worked: the failures in a row start again from none.
    pub(super) fn ok(&mut self) {
        self.failures = 0;
    }

    /// Why the quota is unhealthy, when it is.
    pub(super) fn why(&self) -> Option<&str> {
        self.locked.as_deref().or(self.unhealthy.as_deref())
    }

    /// A finished run's slot was given up on (its usage never read zero). The count is
    /// cumulative and a run that ends well does not reduce it: at the limit the quota is
    /// unavailable until the agent is restarted, which is reported every minute.
    pub(super) fn abandoned(&mut self, quota: &RunQuota, now_ms: u64, events: &mut Vec<Event>) {
        self.abandoned = self.abandoned.saturating_add(1);
        if self.abandoned == quota.abandon_limit() {
            self.next_ms = now_ms.saturating_add(REPROBE_MS);
            let why = Self::locked_text(self.abandoned);
            events.push(Self::locked_event(&why));
            self.locked = Some(why);
        }
    }

    fn locked_text(abandoned: u32) -> String {
        format!(
            "{abandoned} slots were given up on because their usage never read zero \
             (see the quota-usage-abandoned findings; look for files held open on the scratch \
             volume): no run starts here until the agent is restarted"
        )
    }

    fn locked_event(why: &str) -> Event {
        Event::Finding(Finding {
            run_id: None,
            code: code::QUOTA_UNHEALTHY,
            message: format!("the scratch quota is unavailable: {why}"),
        })
    }

    /// A call failed. After `max_failures` in a row the quota is probed; if the probe is not
    /// `Project` the quota is unhealthy from now.
    pub(super) fn failed(
        &mut self,
        quota: &RunQuota,
        now_ms: u64,
        dir: &Path,
        events: &mut Vec<Event>,
    ) {
        self.failures = self.failures.saturating_add(1);
        if self.failures < quota.max_failures() || self.unhealthy.is_some() {
            return;
        }
        match quota.check(dir) {
            Ok(()) => self.failures = 0,
            Err(reason) => {
                self.next_ms = now_ms.saturating_add(REPROBE_MS);
                events.push(Self::report(&reason, self.failures));
                self.unhealthy = Some(reason);
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
    pub(super) fn tick(
        &mut self,
        quota: &RunQuota,
        now_ms: u64,
        dir: &Path,
        events: &mut Vec<Event>,
    ) {
        if now_ms < self.next_ms || (self.unhealthy.is_none() && self.locked.is_none()) {
            return;
        }
        if let Some(why) = &self.locked {
            self.next_ms = now_ms.saturating_add(REPROBE_MS);
            events.push(Self::locked_event(why));
            return;
        }
        self.next_ms = now_ms.saturating_add(REPROBE_MS);
        match quota.check(dir) {
            Ok(()) => {
                self.failures = 0;
                self.unhealthy = None;
                events.push(Event::Finding(Finding {
                    run_id: None,
                    code: code::QUOTA_RECOVERED,
                    message: "the scratch quota works again: runs start here".to_owned(),
                }));
            }
            Err(reason) => {
                events.push(Self::report(&reason, self.failures));
                self.unhealthy = Some(reason);
            }
        }
    }
}
