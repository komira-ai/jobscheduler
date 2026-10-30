//! The per-run scratch disk quota in the lifecycle, over the in-memory quota: what a run
//! books, the order of the calls around a run's scratch directory, and what a failed call, a
//! restart and the sweep do. The mechanism itself is tested in `quota` and, on a real XFS
//! host, in `quota::xfs_host`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser;
use js_agent::Cli;
use js_agent::lifecycle::{Local, RunQuota, label};
use js_agent::quota::{
    Call, FakeQuota, Probe, ProjectId, ProjectIds, QuotaError, ScratchQuota, Usage,
};
use js_agent_wire::{Outcome, RunFile};

use super::{Harness, Options, cleanup, job, names};
use crate::support::Scratch;

/// The bytes a run without `disk_bytes` books in these tests: 2 GiB.
pub const DEFAULT_BYTES: u64 = 2 << 30;

/// The inode limit of a run in these tests.
pub const INODES: u64 = 1000;

const GIB: u64 = 1 << 30;

/// A [`FakeQuota`] that also notes, at every call, what the scratch root looked like. The
/// notes are what the order tests read: whether the directory was empty when its limits
/// were set, and which directories were still in the root when a project was cleared.
pub struct Spy {
    pub inner: FakeQuota,
    seen: Mutex<Vec<String>>,
    applied: Mutex<Vec<PathBuf>>,
    /// The reads of lagging usage still to come for each project (see `set_lag`).
    lagging: Mutex<std::collections::BTreeMap<ProjectId, u32>>,
    lag: Mutex<u32>,
    probe_none: Mutex<Option<String>>,
    fail_usage: Mutex<bool>,
}

impl Spy {
    pub fn new() -> Self {
        Self::over(FakeQuota::new())
    }

    /// A spy over `inner`.
    pub fn over(inner: FakeQuota) -> Self {
        Self {
            inner,
            seen: Mutex::default(),
            applied: Mutex::default(),
            lagging: Mutex::default(),
            lag: Mutex::default(),
            probe_none: Mutex::default(),
            fail_usage: Mutex::default(),
        }
    }

    /// After a successful `clear` of a project whose tree is gone, its usage reads nonzero
    /// for `reads` more reads and then zero: the lag of a file system that frees the inodes
    /// of a removed tree in the background.
    pub fn set_lag(&self, reads: u32) {
        *self.lag.lock().unwrap_or_else(PoisonError::into_inner) = reads;
    }

    /// From now the probe says none for `reason` (a mount that lost `prjquota`); `None`
    /// gives the fake's own answer back.
    pub fn set_probe_none(&self, reason: Option<&str>) {
        *self
            .probe_none
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = reason.map(str::to_owned);
    }

    /// While set, reading a usage fails.
    pub fn set_fail_usage(&self, fail: bool) {
        *self
            .fail_usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fail;
    }

    /// The notes, in the order of the calls: `apply <dir> empty=<bool> ok=<bool>` and
    /// `clear <dir> listed=<names in the root>`.
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the limits of `dir` were set (an `apply` of it succeeded).
    pub fn applied(&self, dir: &Path) -> bool {
        let applied = self.applied.lock().unwrap_or_else(PoisonError::into_inner);
        applied.iter().any(|set| set == dir)
    }

    fn note(&self, line: String) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

fn leaf(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

impl ScratchQuota for Spy {
    fn probe(&self) -> Probe {
        let none = self
            .probe_none
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match none {
            Some(reason) => Probe::none(reason),
            None => self.inner.probe(),
        }
    }

    fn apply(
        &self,
        run_dir: &Path,
        project: ProjectId,
        block_limit_bytes: u64,
        inode_limit: u64,
    ) -> Result<(), QuotaError> {
        let empty = fs::read_dir(run_dir).is_ok_and(|mut entries| entries.next().is_none());
        let done = self
            .inner
            .apply(run_dir, project, block_limit_bytes, inode_limit);
        self.note(format!(
            "apply {} empty={empty} ok={}",
            leaf(run_dir),
            done.is_ok()
        ));
        if done.is_ok() {
            self.applied
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(run_dir.to_owned());
        }
        done
    }

    fn usage(&self, project: ProjectId) -> Result<Usage, QuotaError> {
        let failing = *self
            .fail_usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if failing {
            return Err(QuotaError::Io {
                what: "read the usage",
                source: std::io::Error::other("scripted failure"),
            });
        }
        let mut lagging = self.lagging.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(left) = lagging.get_mut(&project).filter(|left| **left > 0) {
            *left -= 1;
            return Ok(Usage {
                bytes: 4096,
                inodes: 1,
            });
        }
        drop(lagging);
        self.inner.usage(project)
    }

    fn clear(&self, run_dir: &Path, project: ProjectId) -> Result<(), QuotaError> {
        let root = run_dir.parent().unwrap();
        let mut listed: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        listed.sort();
        self.note(format!("clear {} listed={listed:?}", leaf(run_dir)));
        let done = self.inner.clear(run_dir, project);
        // The files of a project are its usage: once the tree is gone, so is the usage.
        if done.is_ok() && !run_dir.exists() {
            self.inner.charge(project, Usage::ZERO);
            let lag = *self.lag.lock().unwrap_or_else(PoisonError::into_inner);
            // Only the first clear starts the lag: a retry of it does not start it again.
            self.lagging
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(project)
                .or_insert(lag);
        }
        done
    }
}

fn project(slot: u32) -> ProjectId {
    ProjectIds::default().of_slot(slot).unwrap()
}

fn with_quota(name: &str) -> (Harness, Arc<Spy>) {
    let spy = Arc::new(Spy::new());
    let options = Options {
        quota: Some(spy.clone()),
        ..Options::default()
    };
    (Harness::with(name, options), spy)
}

fn applies(spy: &Spy) -> Vec<Call> {
    let calls = spy.inner.calls();
    calls
        .into_iter()
        .filter(|call| matches!(call, Call::Apply { .. }))
        .collect()
}

fn clears(spy: &Spy) -> Vec<Call> {
    let calls = spy.inner.calls();
    calls
        .into_iter()
        .filter(|call| matches!(call, Call::Clear { .. }))
        .collect()
}

/// Asserts the order of a run's quota: the limits are set on the empty directory before
/// the container is created (the real runtime gives the directory to the slot's user in the
/// create, so a quota set later would come after the chown, with files possibly written),
/// and at the end the project is cleared only after the tree is gone (the scratch root no
/// longer lists the run) and before the run's cleanup is reported done. It catches an
/// `apply` moved after the container is made or after anything is written, and a `clear`
/// moved before the removal, which would take the bound off a project that still has files.
#[test]
fn apply_comes_before_the_container_and_clear_after_the_tree_is_gone() {
    let (mut h, spy) = with_quota("quota-order");
    h.start(job("r1"));
    assert!(
        h.log
            .borrow()
            .iter()
            .any(|line| line == "create r1 [quota set]"),
        "{:?}",
        h.log.borrow()
    );
    assert_eq!(spy.seen(), ["apply r1 empty=true ok=true"]);
    assert_eq!(
        spy.inner.limits_of(project(0)),
        Some((DEFAULT_BYTES, INODES))
    );
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert_eq!(
        spy.seen(),
        ["apply r1 empty=true ok=true", "clear r1 listed=[]"],
        "the root no longer listed r1 when its project was cleared"
    );
    assert_eq!(spy.inner.limits_of(project(0)), None);
    assert!(
        h.log
            .borrow()
            .iter()
            .any(|line| line == "check r1 (container gone, scratch gone)"),
        "the checks ran after the clear: {:?}",
        h.log.borrow()
    );
}

/// Asserts what a run is limited to and books: a run without `disk_bytes` the default, a run
/// with its own the bytes it named, both written to the container's `disk-bytes` label (so
/// a restart restores them) and both taken out of the offer; a run that no longer fits
/// beside them is refused `no-capacity` before a directory or a project is touched. It
/// catches a quota bound to the spec's number instead of the booked one (a run without disk
/// would be unbounded), and a default that is limited but not booked.
#[test]
fn a_run_is_limited_to_the_bytes_it_books_and_the_default_is_booked() {
    let (mut h, spy) = with_quota("quota-book");
    h.start(job("a"));
    let mut big = job("b");
    big.limits.disk_bytes = Some(3 * GIB);
    h.start(big);
    assert_eq!(h.life.free().disk_bytes, 5 * GIB);
    let bound: Vec<(u64, u64)> = applies(&spy)
        .into_iter()
        .map(|call| match call {
            Call::Apply {
                block_limit_bytes,
                inode_limit,
                ..
            } => (block_limit_bytes, inode_limit),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(bound, [(2 * GIB, INODES), (3 * GIB, INODES)]);
    let labelled = |at: usize| h.fake.created()[at].labels[label::DISK_BYTES].clone();
    assert_eq!(labelled(0), (2 * GIB).to_string());
    assert_eq!(labelled(1), (3 * GIB).to_string());

    let mut fill = job("c");
    fill.limits.disk_bytes = Some(5 * GIB);
    h.start(fill);
    assert_eq!(h.life.free().disk_bytes, 0);
    let before = spy.inner.calls().len();
    assert_eq!(names(&h.start(job("d"))), ["refused d no-capacity"]);
    assert_eq!(
        spy.inner.calls().len(),
        before,
        "the refused run touched no project"
    );
    assert!(!h.scratch_of("d").exists());
}

/// Asserts that a failed `apply` refuses the run and creates nothing: no container, no run
/// files, a result `failed_to_start` with `scratch-failed`; that the directory it was made
/// in is removed and its project cleared by the next step; and that the slot and the booked
/// bytes come back. It catches a run that starts without its bound, a container made after
/// a failed bound, files written before it, and a directory or project left behind.
#[test]
fn a_failed_apply_refuses_the_run_and_creates_nothing() {
    let (mut h, spy) = with_quota("quota-apply-fails");
    spy.inner.set_fail_apply(true);
    let mut spec = job("r1");
    spec.files = vec![RunFile {
        name: "grant".into(),
        contents: "token\n".into(),
    }];
    let events = names(&h.start(spec));
    assert_eq!(events, ["finding r1 scratch-failed", "result r1"]);
    assert!(h.fake.created().is_empty(), "no container");
    let files = h.config.files.as_ref().unwrap();
    assert!(!files.of("r1").exists(), "no run files were written");
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("scratch-failed"));

    spy.inner.set_fail_apply(false);
    h.life.step();
    assert!(!h.scratch_of("r1").exists(), "the directory is removed");
    assert_eq!(clears(&spy).len(), 1, "and its project cleared");
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.free().disk_bytes, 10 * GIB);
    assert_eq!(
        names(&h.start(job("r2"))),
        ["started r2"],
        "the slot is usable"
    );
}

/// Asserts that a `clear` that fails holds the slot and the booked bytes, reports once, does
/// not taint the host (the tree is gone; nothing is left on disk), and is tried again at
/// every step until it works; then the cleanup goes on in order and the slot comes back. It
/// catches a slot freed over a project that still carries limits, a retry that stops, and a
/// failed clear treated as an unremovable tree, which would taint the host for good.
#[test]
fn a_failed_clear_holds_the_slot_and_is_retried() {
    let (mut h, spy) = with_quota("quota-clear-fails");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    spy.inner.set_fail_clear(true);
    assert_eq!(
        names(&h.life.step()),
        [
            "result r1",
            "container removed r1",
            "finding r1 quota-clear-failed"
        ]
    );
    assert_eq!(h.life.busy_slots(), [0]);
    assert_eq!(h.life.free().disk_bytes, 8 * GIB, "still booked");
    assert!(!h.life.is_tainted());
    assert!(names(&h.life.step()).is_empty(), "reported once");
    assert_eq!(clears(&spy).len(), 2, "tried at every step");

    spy.inner.set_fail_clear(false);
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
    assert!(h.life.busy_slots().is_empty());
    assert_eq!(h.life.free().disk_bytes, 10 * GIB);
    assert_eq!(clears(&spy).len(), 3);
    assert_eq!(spy.inner.limits_of(project(0)), None);
}

/// Asserts what the agent does with a slot whose project id still holds usage (what a crash
/// left): the slot is parked and reported, the run that came upon it takes the next free
/// slot and starts (it is not refused), the parked slot takes no run, a step reads it again
/// and keeps it parked while the usage stays, and once it reads zero it is released,
/// reported under a code of its own, and used again. It catches a run started over files
/// that are counted against another run's limits, a run lost to a slot that is not clean
/// while thousands are, a slot that is lost for good, and one released without reading it.
#[test]
fn a_slot_whose_project_holds_usage_is_parked_and_the_run_takes_the_next_one() {
    let (mut h, spy) = with_quota("quota-parked");
    spy.inner.charge(
        project(0),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    assert_eq!(
        names(&h.start(job("r1"))),
        ["finding r1 quota-slot-busy", "started r1"]
    );
    assert_eq!(h.fake.created()[0].slot, 1, "the next free slot");
    assert_eq!(h.life.parked_slots(), [0]);
    assert_eq!(h.life.busy_slots(), [1]);
    assert!(names(&h.life.step()).is_empty());
    assert_eq!(h.life.parked_slots(), [0], "still in use");

    spy.inner.charge(project(0), Usage::ZERO);
    assert_eq!(names(&h.life.step()), ["finding - quota-slot-released"]);
    assert!(h.life.parked_slots().is_empty());
    h.start(job("r2"));
    assert_eq!(
        h.fake.created()[1].slot,
        0,
        "released: the lowest slot again"
    );
}

/// Asserts that a run is refused only when no free slot is clean: with every project in use
/// the run is refused `no-capacity` with nothing created (no directory, no container, no
/// project touched), every slot is parked and reported, and the result carries the code. It
/// catches a refusal while a clean slot exists, and a run started when none is.
#[test]
fn a_run_is_refused_only_when_no_free_slot_is_clean() {
    let (mut h, spy) = with_quota("quota-all-busy");
    let used = Usage {
        bytes: 4096,
        inodes: 2,
    };
    for slot in 0..4096 {
        spy.inner.charge(project(slot), used);
    }
    let events = names(&h.start(job("r1")));
    assert_eq!(events.len(), 4097);
    assert_eq!(
        events.last().map(String::as_str),
        Some("refused r1 no-capacity")
    );
    assert!(
        events[..4096]
            .iter()
            .all(|line| line == "finding r1 quota-slot-busy")
    );
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("r1").exists());
    assert!(applies(&spy).is_empty());
    assert_eq!(h.life.parked_slots().len(), 4096);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("no-capacity"));
}

/// Asserts the lag of a real file system: the usage of a project reads nonzero for a while
/// after its tree was removed and the project cleared. The finished run keeps its slot (and
/// its booked bytes, see the cleanup) and says so once, an immediate next run starts on
/// another slot instead of being refused, the slot is read at every step, and it is freed
/// (and used again) only on the step that reads zero. It catches a slot freed over a project
/// that still counts the old run, which the next run of the slot would be refused for.
#[test]
fn a_finished_slot_is_reused_only_after_its_usage_reads_zero() {
    let (mut h, spy) = with_quota("quota-lag");
    spy.set_lag(3);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    assert_eq!(
        names(&h.life.step()),
        [
            "result r1",
            "container removed r1",
            "finding r1 quota-usage-held"
        ]
    );
    assert_eq!(h.life.busy_slots(), [0], "held while the usage reads");
    assert_eq!(h.life.free().disk_bytes, 8 * GIB);
    assert!(!h.life.is_tainted());

    assert_eq!(names(&h.start(job("r2"))), ["started r2"]);
    assert_eq!(h.fake.created()[1].slot, 1, "the next run did not wait");
    assert!(
        names(&h.life.step()).is_empty(),
        "second read: still in use"
    );
    assert_eq!(h.life.busy_slots(), [0, 1]);
    assert!(names(&h.life.step()).is_empty(), "third read: still in use");
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"],
        "the fourth read is zero"
    );
    assert_eq!(h.life.busy_slots(), [1]);
    h.start(job("r3"));
    assert_eq!(h.fake.created()[2].slot, 0, "reused after the lag");
}

/// Asserts that a usage that cannot be read after the clear holds the slot like a failed
/// clear (finding `quota-clear-failed`, no taint) and is read again at the next step. It
/// catches a slot freed over a project nobody could read.
#[test]
fn a_usage_that_cannot_be_read_at_the_end_holds_the_slot() {
    let (mut h, spy) = with_quota("quota-usage-fails");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    spy.set_fail_usage(true);
    assert_eq!(
        names(&h.life.step()),
        [
            "result r1",
            "container removed r1",
            "finding r1 quota-clear-failed"
        ]
    );
    assert_eq!(h.life.busy_slots(), [0]);
    assert!(!h.life.is_tainted());
    spy.set_fail_usage(false);
    assert_eq!(
        names(&h.life.step()),
        ["scratch removed r1", "checks passed r1", "freed r1"]
    );
}

/// Asserts the quota's health: calls that fail in a row (`apply` here) do not stop the
/// agent while the probe still says project (the counter starts again); once the probe says
/// none, the next run of failures makes the quota unhealthy at the third failure, reported
/// once, and every start is then refused `no-capacity` with nothing created; the report
/// comes again after a minute, not before; and when the probe says project again a step
/// reports the recovery and runs start again. It catches a host that keeps taking runs it
/// cannot bound, one that never takes runs again, and a probe on every failure.
#[test]
fn a_quota_that_fails_in_a_row_and_probes_none_stops_taking_runs_until_it_recovers() {
    let (mut h, spy) = with_quota("quota-health");
    spy.inner.set_fail_apply(true);
    for run in ["w1", "w2", "w3", "w4"] {
        assert_eq!(
            names(&h.start(job(run))),
            [
                format!("finding {run} scratch-failed"),
                format!("result {run}")
            ],
            "a working probe: only the run fails"
        );
    }
    h.life.step();
    spy.set_probe_none(Some("remounted without prjquota"));
    h.start(job("a"));
    h.start(job("b"));
    assert_eq!(
        names(&h.start(job("c"))),
        [
            "finding - quota-unhealthy",
            "finding c scratch-failed",
            "result c"
        ]
    );
    assert_eq!(names(&h.start(job("d"))), ["refused d no-capacity"]);
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("d").exists());
    assert_eq!(
        h.life.results().last().unwrap().detail.as_deref(),
        Some("no-capacity")
    );
    assert!(!h.life.is_tainted());

    h.life.step();
    h.clock.advance(59_999);
    assert!(!names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    h.clock.advance(1);
    assert_eq!(names(&h.life.step()), ["finding - quota-unhealthy"]);

    spy.set_probe_none(None);
    spy.inner.set_fail_apply(false);
    h.clock.advance(60_000);
    assert_eq!(names(&h.life.step()), ["finding - quota-recovered"]);
    assert_eq!(names(&h.start(job("e"))), ["started e"]);
}

/// Asserts that failed `clear` calls count toward the health as `apply` does: three steps
/// that fail to clear, on a probe that says none, make the quota unhealthy at the third.
#[test]
fn clear_failures_in_a_row_make_the_quota_unhealthy() {
    let (mut h, spy) = with_quota("quota-health-clear");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    spy.inner.set_fail_clear(true);
    spy.set_probe_none(Some("remounted without prjquota"));
    h.life.step();
    assert!(!names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    assert!(names(&h.life.step()).contains(&"finding - quota-unhealthy".to_owned()));
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 no-capacity"]);
}

/// Asserts that slots that stay parked are reported again after an hour and not before. It
/// catches a slot that is lost to the operator after one line in a log.
#[test]
fn a_slot_that_stays_parked_is_reported_again_every_hour() {
    let (mut h, spy) = with_quota("quota-remind");
    spy.inner.charge(
        project(0),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    h.start(job("r1"));
    h.life.step();
    h.clock.advance(3_599_999);
    assert!(names(&h.life.step()).is_empty());
    h.clock.advance(1);
    assert_eq!(names(&h.life.step()), ["finding - quota-slot-busy"]);
    assert!(names(&h.life.step()).is_empty(), "once an hour");
}

/// Asserts that a restart with another `--scratch-default-gib` leaves an adopted run's
/// booked bytes as its label says (the default it started with), and books a new run
/// without disk at the new default. It catches an adoption that re-books at today's
/// default, which would free or add disk the run never had.
#[test]
fn a_changed_default_keeps_the_booking_of_adopted_runs() {
    let (mut h, spy) = with_quota("quota-default-change");
    h.start(job("r1"));
    h.config.quota = Some(RunQuota::new(spy.clone(), 5 * GIB, INODES));
    assert_eq!(names(&h.restart().unwrap()), ["adopted r1"]);
    assert_eq!(h.life.free().disk_bytes, 8 * GIB, "r1 keeps its 2 GiB");
    h.start(job("r2"));
    assert_eq!(h.life.free().disk_bytes, 3 * GIB, "r2 books the new 5 GiB");
    let labelled = |at: usize| h.fake.created()[at].labels[label::DISK_BYTES].clone();
    assert_eq!(labelled(0), (2 * GIB).to_string());
    assert_eq!(labelled(1), (5 * GIB).to_string());
}

/// Asserts a restart in the middle of a run: the new process adopts the run with its slot and
/// its directory, does not set or clear anything for it (its slot is not scanned and its
/// limits stay), and when the run ends under the new process its project is cleared after
/// the tree is removed, with the same project it started with. It catches an adoption that
/// clears a live run's bound (the job would run unbounded), a sweep that removes the live
/// directory, and a run that ends without releasing its project.
#[test]
fn a_restart_in_the_middle_of_a_run_keeps_its_bound_and_clears_it_at_the_end() {
    let (mut h, spy) = with_quota("quota-restart");
    h.start(job("r1"));
    spy.inner.charge(
        project(0),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    let before = spy.inner.calls().len();
    assert_eq!(names(&h.restart().unwrap()), ["adopted r1"]);
    let after = spy.inner.calls().split_off(before);
    assert!(
        after.iter().all(|call| matches!(call, Call::Usage(_))),
        "the restart only reads"
    );
    assert!(
        !after.contains(&Call::Usage(project(0))),
        "the held slot is not looked at"
    );
    assert!(h.scratch_of("r1").exists(), "the live directory stays");
    assert_eq!(h.life.busy_slots(), [0]);
    assert!(h.life.parked_slots().is_empty());
    assert_eq!(
        spy.inner.limits_of(project(0)),
        Some((DEFAULT_BYTES, INODES))
    );
    assert_eq!(
        h.life.free().disk_bytes,
        8 * GIB,
        "its booking was read back"
    );

    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    let cleared = clears(&spy);
    assert_eq!(cleared.len(), 1);
    assert!(matches!(&cleared[0], Call::Clear { project: p, .. } if *p == project(0)));
    assert_eq!(spy.inner.limits_of(project(0)), None);
}

/// Asserts that the sweep at a restart clears the projects of the directories it removes:
/// a leftover directory (a run of an earlier process whose container is gone) on a slot
/// whose project holds usage is removed, and only then is that project cleared (the root no
/// longer lists it), once, while no other project is touched; the slot is then free. It
/// catches leftover limits and project ids that outlive their tree, a clear before the
/// removal, and a sweep that clears every one of the thousands of projects.
#[test]
fn the_sweep_clears_the_projects_of_the_directories_it_removes() {
    let (mut h, spy) = with_quota("quota-sweep");
    let old = h.scratch_of("old");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("left"), "x").unwrap();
    spy.inner.charge(
        project(5),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    let events = names(&h.restart().unwrap());
    assert!(events.is_empty(), "{events:?}");
    assert!(!old.exists());
    let cleared = clears(&spy);
    assert_eq!(cleared.len(), 1);
    assert!(matches!(&cleared[0], Call::Clear { project: p, .. } if *p == project(5)));
    assert_eq!(
        spy.seen().last().map(String::as_str),
        Some("clear .js-swept listed=[]"),
        "the leftover was gone when its project was cleared"
    );
    assert!(h.life.parked_slots().is_empty());
}

/// Asserts that a leftover whose project cannot be cleared at a restart parks its slot and
/// says so, that the slot takes no run while its usage stays, and that it is released by a
/// step once the usage reads zero. It catches a leftover project that a later run would
/// inherit, with its files counted against the new run.
#[test]
fn a_leftover_that_cannot_be_cleared_parks_its_slot() {
    let (mut h, spy) = with_quota("quota-sweep-fails");
    fs::create_dir(h.scratch_of("old")).unwrap();
    spy.inner.charge(
        project(5),
        Usage {
            bytes: 4096,
            inodes: 2,
        },
    );
    spy.inner.set_fail_clear(true);
    assert_eq!(names(&h.restart().unwrap()), ["finding - quota-slot-busy"]);
    assert_eq!(h.life.parked_slots(), [5]);
    for run in ["a", "b", "c", "d", "e", "f"] {
        // Small runs, so that six fit the harness's four CPUs and ten GiB.
        let mut small = job(run);
        small.limits.cpu_millis = 100;
        small.limits.disk_bytes = Some(1 << 20);
        h.start(small);
    }
    let slots: Vec<u32> = h.fake.created().iter().map(|spec| spec.slot).collect();
    assert_eq!(slots, [0, 1, 2, 3, 4, 6], "slot 5 was skipped");

    spy.inner.charge(project(5), Usage::ZERO);
    assert_eq!(names(&h.life.step()), ["finding - quota-slot-released"]);
    assert!(h.life.parked_slots().is_empty());
}

/// The job flags `extra` beside `--scratch-quota`, checked as the agent checks them, over a
/// private scratch root of its own.
pub fn local_with(name: &str, extra: &[&str]) -> Local {
    let dir = Scratch::new(name);
    let root = dir.path("scratch");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let root = root.display().to_string();
    let mut argv = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        "p",
        "--scratch-root",
        &root,
        "--scratch-quota",
    ];
    argv.extend(extra.iter().copied());
    Local::from_cli(&Cli::try_parse_from(argv).unwrap()).unwrap()
}

/// Asserts that the flags reach the quota: `--scratch-default-gib` is what a run without
/// `disk_bytes` books and is bounded to, `--scratch-max-inodes` is its inode limit, and
/// without them the defaults are 2 GiB and 1,000,000 inodes. It catches a flag that is
/// checked and then dropped, and two numbers swapped on the way to the quota.
#[test]
fn the_flags_set_the_default_and_the_inode_limit_of_every_run() {
    for (extra, gib, inodes) in [
        (vec![], 2, 1_000_000),
        (vec!["--scratch-default-gib", "5"], 5, 1_000_000),
        (vec!["--scratch-max-inodes", "77"], 2, 77),
        (
            vec!["--scratch-default-gib", "7", "--scratch-max-inodes", "9"],
            7,
            9,
        ),
    ] {
        let local = local_with("quota-flags", &extra);
        let fake = Arc::new(FakeQuota::new());
        let quota = local.run_quota(fake.clone());
        assert_eq!(quota.default_disk_bytes(), gib * GIB);
        let shown = format!("{quota:?}");
        assert!(
            shown.contains(&format!("default_disk_bytes: {}", gib * GIB)),
            "{shown}"
        );
        assert_eq!(quota.max_inodes(), inodes);

        let mut h = Harness::new("quota-flags-run");
        h.config.quota = Some(quota);
        h.restart().unwrap();
        h.start(job("r1"));
        let Call::Apply {
            block_limit_bytes,
            inode_limit,
            ..
        } = &applies_of(&fake)[0]
        else {
            panic!("not an apply");
        };
        assert_eq!((*block_limit_bytes, *inode_limit), (gib * GIB, inodes));
    }
}

/// Asserts that `--scratch-quota-failures` reaches the quota, and is 3 without it.
#[test]
fn the_failure_flag_reaches_the_quota() {
    let fake = || Arc::new(FakeQuota::new());
    let default = local_with("quota-failures-default", &[]).run_quota(fake());
    assert_eq!(default.max_failures(), 3);
    let five = local_with("quota-failures-five", &["--scratch-quota-failures", "5"]);
    assert_eq!(five.run_quota(fake()).max_failures(), 5);
    assert_eq!(
        RunQuota::new(fake(), GIB, 1)
            .with_max_failures(0)
            .max_failures(),
        1
    );
}

fn applies_of(fake: &FakeQuota) -> Vec<Call> {
    fake.calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Apply { .. }))
        .collect()
}

/// Asserts that without a quota nothing is set, cleared or read, a run without `disk_bytes`
/// books none, and the `disk-bytes` label says 0: an agent started without `--scratch-quota`
/// behaves as before. It catches a default booked, or a quota call made, on a host that was
/// never asked for enforcement.
#[test]
fn without_a_quota_a_run_books_no_default_and_no_call_is_made() {
    let mut h = Harness::new("quota-off");
    assert!(h.config.quota.is_none());
    h.start(job("r1"));
    assert_eq!(h.life.free().disk_bytes, 10 * GIB);
    assert_eq!(h.fake.created()[0].labels[label::DISK_BYTES], "0");
    h.fake.exit("r1", 0);
    assert_eq!(
        names(&h.life.step()),
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
}

/// Asserts what a quota whose usage cannot be read does (the file system lost its quota after
/// the start): a restart reports once that leftovers are not cleared and stops looking, and a
/// run is refused `scratch-failed` (not `no-capacity`: the host is at fault, not the slot's
/// files), with nothing created and its slot parked. It catches a run started with nothing
/// known about what its project holds, and a restart that goes on reading 4096 failing
/// slots.
#[test]
fn a_quota_that_cannot_read_usage_refuses_runs_and_parks_the_slot() {
    let spy = Arc::new(Spy::over(FakeQuota::unsupported(
        "the file system lost its quota",
    )));
    let options = Options {
        quota: Some(spy.clone()),
        ..Options::default()
    };
    let mut h = Harness::with("quota-unreadable", options);
    let events = h.restart().unwrap();
    assert_eq!(names(&events), ["finding - quota-slot-busy"]);
    let js_agent::lifecycle::Event::Finding(found) = &events[0] else {
        panic!("not a finding");
    };
    assert!(
        found.message.contains("cannot be read") && found.message.contains("lost its quota"),
        "{}",
        found.message
    );
    assert_eq!(
        names(&h.start(job("r1"))),
        ["finding r1 quota-slot-busy", "refused r1 scratch-failed"]
    );
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("r1").exists());
    assert_eq!(h.life.parked_slots(), [0]);
}
