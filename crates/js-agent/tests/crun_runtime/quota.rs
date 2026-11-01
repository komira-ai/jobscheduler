//! The per-run disk quota over a real `crun` and a real XFS project quota: one bound covers
//! what a job writes to `/scratch`, to the `/work` overlay and to `/root`, and the kernel
//! refuses a write past it with "no space left" while the volume itself keeps its room.
//!
//! The volume is a loop-mounted XFS with `prjquota` (`JS_XFS_ROOT`, made by the workflow). The
//! tests do what the lifecycle does around a run, in its order: the run's directory and the
//! runtime's lease directory ([`Runtime::lease_dir`]) are made empty beside each other, both
//! are put under one project with one limit, and only then is the container created. Each test
//! takes a slot of its own, so the projects of tests that run at once are not shared.
//!
//! An XFS project quota that is hit answers `ENOSPC`, not `EDQUOT`, so a refusal is read from
//! the job's own words ("No space left on device"), its exit status, and the project's usage.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::{Body, Status};
use js_agent::quota::{Mode, ProjectId, ProjectIds, ScratchQuota, XfsQuota};
use js_contain::oci::LEASE_PREFIX;

use crate::runtime_suite::{Program, Shared, Subject, spec_of};
use crate::subject::{CrunSubject, crun_subject_on, var};

/// The limit of a run's project: what `/scratch`, `/work` and `/root` share.
const LIMIT: u64 = 8 << 20;

/// The inode limit of a run's project.
const INODES: u64 = 10_000;

/// What a job that must be refused tries to write, far above [`LIMIT`].
const ATTEMPT_MIB: u64 = 96;

/// The room the volume may lose to a bounded job and to the other tests that run at once on
/// it (their image stores and bounded writes): well below [`ATTEMPT_MIB`].
const VOLUME_SLACK: u64 = 48 << 20;

/// The id of the run every test starts.
const RUN: &str = "w1";

/// The mount point of the XFS volume the workflow made.
fn volume() -> PathBuf {
    let root = PathBuf::from(var("JS_XFS_ROOT"));
    fs::canonicalize(&root).unwrap_or_else(|e| panic!("JS_XFS_ROOT {}: {e}", root.display()))
}

/// The room left on the volume, in bytes.
fn free_bytes(volume: &Path) -> u64 {
    let stat = rustix::fs::statvfs(volume).expect("statvfs the volume");
    stat.f_bavail.saturating_mul(stat.f_frsize)
}

/// What the quota is cleaned up with, whatever a test did: the containers are stopped and
/// removed, the directories removed, and the bound taken off the project. It is declared
/// after the subject, so it drops first.
struct Bound {
    quota: XfsQuota,
    runtime: Shared,
    run_dir: PathBuf,
    lease_dir: PathBuf,
    project: ProjectId,
}

impl Drop for Bound {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.runtime.try_borrow_mut()
            && let Ok(found) = runtime.list_labelled()
        {
            for container in found {
                let _ = runtime.stop(&container.id, Duration::ZERO);
                let _ = runtime.remove(&container.id);
            }
        }
        let _ = fs::remove_dir_all(&self.lease_dir);
        let _ = fs::remove_dir_all(&self.run_dir);
        let _ = self.quota.clear(&self.run_dir, self.project);
    }
}

/// What a job that wrote to one place left behind.
struct Written {
    exit: i32,
    stderr: String,
    used: u64,
    volume_lost: u64,
    /// Whether the lease is in the image store (it must not be: only the scratch root).
    lease_in_store: bool,
}

/// Runs a job that writes `mib` MiB to `target` (`/scratch`, `/work` or `/root`) under one
/// project of `slot` limited to [`LIMIT`], waits for it to end and reads what it left.
fn write_to(target: &str, mib: u64, slot: u32) -> Written {
    let volume = volume();
    let mut subject: CrunSubject = crun_subject_on(&format!("quota_{slot}"), &volume);
    subject.prepared();
    let mut spec = spec_of(&mut subject, RUN, Program::Exit(0));
    let script =
        format!("dd if=/dev/zero of={target}/fill bs=1048576 count={mib} && exit 0; exit 7");
    let Body::Container { args, .. } = &mut spec.body else {
        panic!("spec_of makes a container job");
    };
    *args = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];

    let quota = XfsQuota::new(subject.runs(), ProjectIds::default());
    let probe = quota.probe();
    assert_eq!(probe.mode, Mode::Project, "{}", probe.reason);
    let project = ProjectIds::default().of_slot(slot).expect("a project");
    let lease_dir = subject
        .runtime()
        .borrow()
        .lease_dir(RUN)
        .expect("the runtime keeps its lease beside the runs");
    let lease_name = lease_dir
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(LEASE_PREFIX))
        .expect("a lease directory is named with the lease prefix")
        .to_owned();
    let bound = Bound {
        quota,
        runtime: subject.runtime(),
        run_dir: spec.scratch.clone(),
        lease_dir: lease_dir.clone(),
        project,
    };
    // The lifecycle's order: both directories empty, both bound, then the container.
    fs::create_dir(&lease_dir).expect("make the lease directory");
    for dir in [&spec.scratch, &lease_dir] {
        bound
            .quota
            .apply(dir, project, LIMIT, INODES)
            .unwrap_or_else(|e| panic!("bound {}: {e}", dir.display()));
    }

    let before = free_bytes(&volume);
    let id = subject
        .runtime()
        .borrow_mut()
        .create_and_start(&spec)
        .expect("create and start");
    subject.settled(&id);
    let status = subject.runtime().borrow_mut().poll(&id).expect("poll");
    let Status::Exited(exit) = status else {
        panic!("the job did not end: {status:?}");
    };
    let tail = subject
        .runtime()
        .borrow_mut()
        .log_tail(&id)
        .expect("log tail")
        .expect("the container is held");
    let used = bound.quota.usage(project).expect("usage").bytes;
    let written = Written {
        exit: exit.code,
        stderr: tail.stderr,
        used,
        volume_lost: before.saturating_sub(free_bytes(&volume)),
        lease_in_store: subject.lease_dir(&lease_name).exists(),
    };
    drop(bound);
    written
}

/// Asserts, for one place, that a job writing far past the limit is refused: it exits with the
/// failure of its `dd`, says the disk is full, and what it wrote is held to the limit; the
/// volume lost far less than the job tried to write. It catches a lease directory outside the
/// project, where `/work` and `/root` are written until the volume is full.
fn refused(target: &str, slot: u32) {
    let w = write_to(target, ATTEMPT_MIB, slot);
    assert_eq!(w.exit, 7, "the write was not refused: {}", w.stderr);
    assert!(
        w.stderr.contains("No space left on device"),
        "not refused as a full disk: {}",
        w.stderr
    );
    assert!(w.used > LIMIT / 2, "the bytes were not counted: {}", w.used);
    assert!(w.used <= LIMIT, "the limit was passed: {}", w.used);
    assert!(
        w.volume_lost <= VOLUME_SLACK,
        "the volume lost {} bytes",
        w.volume_lost
    );
    assert!(!w.lease_in_store, "the lease is in the image store");
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_work_is_refused() {
    refused("/work", 20);
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_root_is_refused() {
    refused("/root", 21);
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_scratch_is_refused() {
    refused("/scratch", 22);
}

/// Asserts that the project's usage counts each of the three places: a job that writes 2 MiB
/// to only one of them, and ends well, leaves at least that much in the project. It catches a
/// usage that reads `/scratch` alone, which the cleanup would believe empty while `/work` or
/// `/root` still held a run's bytes.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn the_usage_of_a_project_counts_scratch_work_and_root() {
    for (slot, target) in [(30, "/scratch"), (31, "/work"), (32, "/root")] {
        let w = write_to(target, 2, slot);
        assert_eq!(w.exit, 0, "{target}: {}", w.stderr);
        assert!(w.used >= 2 << 20, "{target}: usage {} bytes", w.used);
        assert!(w.used <= LIMIT, "{target}: usage {} bytes", w.used);
    }
}

/// Asserts that a lease directory outside the run's project is refused: the run's directory is
/// bound and the lease directory the lifecycle made beside it is not, so the runtime refuses to
/// make the container, says why, and holds none. It catches a runtime that would run a job
/// whose `/work` and `/root` lie outside the bound.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_lease_directory_outside_the_runs_project_is_refused() {
    let volume = volume();
    let mut subject: CrunSubject = crun_subject_on("quota_outside", &volume);
    subject.prepared();
    let spec = spec_of(&mut subject, RUN, Program::Exit(0));
    let quota = XfsQuota::new(subject.runs(), ProjectIds::default());
    let project = ProjectIds::default().of_slot(40).expect("a project");
    let lease_dir = subject
        .runtime()
        .borrow()
        .lease_dir(RUN)
        .expect("the runtime keeps its lease beside the runs");
    let bound = Bound {
        quota,
        runtime: subject.runtime(),
        run_dir: spec.scratch.clone(),
        lease_dir: lease_dir.clone(),
        project,
    };
    fs::create_dir(&lease_dir).expect("make the lease directory");
    bound
        .quota
        .apply(&spec.scratch, project, LIMIT, INODES)
        .expect("bound the run's directory");
    let refused = subject.runtime().borrow_mut().create_and_start(&spec);
    let why = refused.expect_err("a lease outside the project is refused");
    assert!(
        why.to_string().contains("not in the run's project"),
        "{why}"
    );
    let held = subject
        .runtime()
        .borrow_mut()
        .list_labelled()
        .expect("list");
    assert!(held.is_empty(), "a container was made");
    drop(bound);
}
