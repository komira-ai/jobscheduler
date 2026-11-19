//! The per-run disk quotas as the lifecycle uses them: what a run books when it names no
//! disk, and the calls on the quota around a run's scratch directory and its job's temporary
//! directory.
//!
//! The mechanism and the order of its calls are in [`crate::quota`]. This is the half that
//! knows about runs and slots: a slot's two project ids (one for the run's scratch, one for
//! its job's `/tmp` and `/var/tmp`), the bytes a run's scratch is bounded to (the bytes it
//! books, so the agent and the server count the same number), the bytes of a job's temporary
//! directory, and the inode limits.

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

/// The inode limit of a job's temporary directory.
pub const TMP_INODES: u64 = 1_000_000;

/// The quota of the runs: the bound of each job's temporary directory, which an agent whose
/// runtime makes one always has, and, with `--scratch-quota` (whose probe passed), the bound
/// of each run's scratch.
#[derive(Clone)]
pub struct RunQuota {
    quota: Arc<dyn ScratchQuota>,
    ids: ProjectIds,
    /// Whether each run's scratch is bounded too (`--scratch-quota`).
    bounds_scratch: bool,
    /// Whether each job's temporary directory is bounded (a runtime that gives one).
    bounds_tmp: bool,
    default_disk_bytes: u64,
    max_inodes: u64,
    tmp_bytes: u64,
    max_failures: u32,
    usage_wait_ms: u64,
    abandon_limit: u32,
    /// Why the quota could not be used when the agent started (its probe was not project):
    /// the quota is unhealthy from the start and no run starts until it works.
    unusable: Option<String>,
}

impl fmt::Debug for RunQuota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunQuota")
            .field("ids", &self.ids)
            .field("bounds_scratch", &self.bounds_scratch)
            .field("bounds_tmp", &self.bounds_tmp)
            .field("default_disk_bytes", &self.default_disk_bytes)
            .field("max_inodes", &self.max_inodes)
            .field("tmp_bytes", &self.tmp_bytes)
            .field("max_failures", &self.max_failures)
            .field("usage_wait_ms", &self.usage_wait_ms)
            .field("abandon_limit", &self.abandon_limit)
            .field("unusable", &self.unusable)
            .finish_non_exhaustive()
    }
}

impl RunQuota {
    /// A quota that bounds a run's scratch to the bytes it books and to `max_inodes`, and
    /// books `default_disk_bytes` for a run that names no disk (the bytes of
    /// `scratch-default-gib`), as `--scratch-quota` asks; no job's temporary directory is
    /// bounded until [`RunQuota::with_tmp_bytes`] says so. The limits are checked by each
    /// `apply`.
    pub fn new(quota: Arc<dyn ScratchQuota>, default_disk_bytes: u64, max_inodes: u64) -> Self {
        Self {
            quota,
            ids: ProjectIds::default(),
            bounds_scratch: true,
            bounds_tmp: false,
            default_disk_bytes,
            max_inodes,
            tmp_bytes: super::DEFAULT_JOB_TMP_GIB << 30,
            max_failures: super::DEFAULT_SCRATCH_QUOTA_FAILURES,
            usage_wait_ms: super::DEFAULT_SCRATCH_USAGE_WAIT_SECS * 1000,
            abandon_limit: super::DEFAULT_SCRATCH_ABANDON_LIMIT,
            unusable: None,
        }
    }

    /// A quota that bounds only each job's temporary directory, to
    /// [`DEFAULT_JOB_TMP_GIB`](super::DEFAULT_JOB_TMP_GIB): a run's scratch is not bounded
    /// and a run without `disk_bytes` books none (a crun agent without `--scratch-quota`).
    pub fn tmp_only(quota: Arc<dyn ScratchQuota>) -> Self {
        Self {
            bounds_scratch: false,
            bounds_tmp: true,
            default_disk_bytes: 0,
            ..Self::new(quota, 0, super::DEFAULT_SCRATCH_MAX_INODES)
        }
    }

    /// The same quota with each job's temporary directory bounded to `bytes`, in the slot's
    /// temporary project ([`ProjectIds::of_tmp_slot`]): a slot is claimed only when that
    /// project reads no usage either, and its bound comes off with the scratch's.
    pub fn with_tmp_bytes(self, bytes: u64) -> Self {
        Self {
            bounds_tmp: true,
            tmp_bytes: bytes,
            ..self
        }
    }

    /// The same quota with no job's temporary directory bounded (a runtime that gives none).
    pub fn without_tmp(self) -> Self {
        Self {
            bounds_tmp: false,
            ..self
        }
    }

    /// Whether each job's temporary directory is bounded.
    pub fn bounds_tmp(&self) -> bool {
        self.bounds_tmp
    }

    /// The same quota, found unusable when the agent started, for `reason` (the probe of the
    /// scratch root was not project): no run starts until a later probe passes.
    pub fn unusable(self, reason: impl Into<String>) -> Self {
        Self {
            unusable: Some(reason.into()),
            ..self
        }
    }

    /// Whether each run's scratch is bounded, as `--scratch-quota` asks.
    pub fn bounds_scratch(&self) -> bool {
        self.bounds_scratch
    }

    /// The bytes each job's temporary directory is bounded to.
    pub fn tmp_bytes(&self) -> u64 {
        self.tmp_bytes
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

    /// The bytes a run without `disk_bytes` books: none when the run's scratch is not
    /// bounded.
    pub fn default_disk_bytes(&self) -> u64 {
        self.default_disk_bytes
    }

    /// The inode limit of each run.
    pub fn max_inodes(&self) -> u64 {
        self.max_inodes
    }

    /// The projects of `slot` the quota uses: the scratch's, and the temporary directory's
    /// when that is bounded.
    fn projects(&self, slot: u32) -> Result<Vec<ProjectId>, QuotaError> {
        let mut projects = vec![self.ids.of_slot(slot)?];
        if self.bounds_tmp {
            projects.push(self.ids.of_tmp_slot(slot)?);
        }
        Ok(projects)
    }

    /// The scratch project of `slot`, if none of the slot's projects holds usage (see
    /// [`ProjectIds::claim`]): a run that takes the slot is bounded by them.
    pub(super) fn claim(&self, slot: u32) -> Result<ProjectId, QuotaError> {
        for project in self.projects(slot)? {
            crate::quota::clean(&*self.quota, project)?;
        }
        self.ids.of_slot(slot)
    }

    /// Bounds the empty scratch directory `dir` of the run in `slot` to `bytes` and the inode
    /// limit, when the run's scratch is bounded; otherwise nothing is done.
    pub(super) fn apply(&self, dir: &Path, slot: u32, bytes: u64) -> Result<(), QuotaError> {
        if !self.bounds_scratch {
            return Ok(());
        }
        let project = self.ids.of_slot(slot)?;
        self.quota.apply(dir, project, bytes, self.max_inodes)
    }

    /// Bounds the empty temporary directory `dir` of the job in `slot` to the bytes of
    /// [`RunQuota::tmp_bytes`] and [`TMP_INODES`], in the slot's own temporary project.
    pub(super) fn apply_tmp(&self, dir: &Path, slot: u32) -> Result<(), QuotaError> {
        if !self.bounds_tmp {
            return Ok(());
        }
        let project = self.ids.of_tmp_slot(slot)?;
        self.quota.apply(dir, project, self.tmp_bytes, TMP_INODES)
    }

    /// Takes the bounds of `slot`'s projects off, after the trees are removed: the
    /// temporary project's from `tmp` (when that is bounded), then the scratch project's from
    /// `dir`. The first failure is returned and the call may be made again.
    pub(super) fn clear(&self, dir: &Path, tmp: &Path, slot: u32) -> Result<(), QuotaError> {
        if self.bounds_tmp {
            self.quota.clear(tmp, self.ids.of_tmp_slot(slot)?)?;
        }
        self.quota.clear(dir, self.ids.of_slot(slot)?)
    }

    /// What `slot`'s projects hold now, together.
    pub(super) fn usage(&self, slot: u32) -> Result<Usage, QuotaError> {
        let mut total = Usage::ZERO;
        for project in self.projects(slot)? {
            let used = self.quota.usage(project)?;
            total = Usage {
                bytes: total.bytes.saturating_add(used.bytes),
                inodes: total.inodes.saturating_add(used.inodes),
            };
        }
        Ok(total)
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
    /// The health of `quota` when the agent starts: unhealthy, and reported, when the quota
    /// was found unusable ([`RunQuota::unusable`]), so no run starts on a host whose jobs'
    /// temporary directories cannot be bounded; it is probed again every [`REPROBE_MS`].
    pub(super) fn at_start(quota: Option<&RunQuota>, now_ms: u64, events: &mut Vec<Event>) -> Self {
        let Some(reason) = quota.and_then(|quota| quota.unusable.clone()) else {
            return Self::default();
        };
        let reason = format!(
            "the scratch root cannot enforce a project quota, which bounds each job's /tmp and \
             /var/tmp: {reason}"
        );
        events.push(Event::Finding(Finding {
            run_id: None,
            code: code::QUOTA_UNHEALTHY,
            message: format!(
                "the scratch quota is unavailable: {reason}: no run starts here until it works"
            ),
        }));
        Self {
            unhealthy: Some(reason),
            next_ms: now_ms.saturating_add(REPROBE_MS),
            ..Self::default()
        }
    }

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
