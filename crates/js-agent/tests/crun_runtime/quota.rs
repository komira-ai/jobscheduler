//! The per-run disk quota over a real `crun` and a real XFS project quota: one bound covers
//! what a job writes to `/scratch`, to the `/work` overlay and to `/root`, the kernel refuses
//! a write past it with "no space left", and nothing the job wrote escapes the run's project.
//!
//! The volume is a loop-mounted XFS with `prjquota` (`JS_XFS_ROOT`, made by the workflow). The
//! tests do what the lifecycle does around a run, in its order: the run's directory and the
//! runtime's lease directory ([`Runtime::lease_dir`]) are made empty beside each other, both
//! are put under one project with one limit, and only then is the container created. Each test
//! takes a slot of its own, so the projects of tests that run at once are not shared.
//!
//! An XFS project quota that is hit answers `ENOSPC`, not `EDQUOT`, so a refusal is read from
//! the job's own words ("No space left on device"), its exit status, and the project's usage.
//!
//! What escaped the project is measured on the run's own directories
//! ([`escape::escaped`]), which back the job's `/scratch`, `/work` and `/root`: the bytes of
//! every entry there that is not in the run's project, which must be 0. The suite's read-write
//! sync mount is outside the bound by design and is not walked; these jobs do not write to it.
//! The volume's free space is not read at all: other tests on the volume, the image store and
//! the file system's own reservations move it, and a correct run must not fail for that.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::{Body, Status};
use js_agent::quota::{Mode, ProjectId, ProjectIds, ScratchQuota, XfsQuota};
use js_contain::oci::LEASE_PREFIX;

use crate::escape::{self, Disk};
use crate::runtime_suite::{Program, Shared, Subject, spec_of};
use crate::subject::{CrunSubject, crun_subject_on, var};

/// The limit of a run's project: what `/scratch`, `/work` and `/root` share.
const LIMIT: u64 = 8 << 20;

/// The inode limit of a run's project.
const INODES: u64 = 10_000;

/// What a job that must be refused tries to write, far above [`LIMIT`].
const ATTEMPT_MIB: u64 = 96;

/// What a neighbour writes to the volume outside the run, while the run is held: as much as
/// the job tries to write, so any bound on the volume's free space that would catch a whole
/// escape is reached by the neighbour alone.
const NEIGHBOUR: u64 = ATTEMPT_MIB << 20;

/// The id of the run every test starts.
const RUN: &str = "w1";

/// The mount point of the XFS volume the workflow made.
pub fn volume() -> PathBuf {
    let root = PathBuf::from(var("JS_XFS_ROOT"));
    fs::canonicalize(&root).unwrap_or_else(|e| panic!("JS_XFS_ROOT {}: {e}", root.display()))
}

/// The device of the volume.
fn dev_of(volume: &Path) -> u64 {
    fs::metadata(volume).expect("stat the volume").dev()
}

/// What the run's directories `dirs` hold outside the project `project`, read on disk.
fn escaped(volume: &Path, dirs: &[&Path], project: ProjectId) -> u64 {
    escape::escaped(&Disk, dev_of(volume), dirs, project.get())
        .unwrap_or_else(|e| panic!("walk the run's directories: {e}"))
}

/// Writes `bytes` of zeros to the new file `path` and syncs it, so the volume holds them.
fn fill(path: &Path, bytes: u64) {
    let block = vec![0_u8; 1 << 20];
    let mut file = fs::File::create_new(path).expect("make the neighbour's file");
    for _ in 0..bytes.div_ceil(block.len() as u64) {
        file.write_all(&block).expect("write the neighbour's file");
    }
    file.sync_all().expect("sync the neighbour's file");
}

/// What the quota is cleaned up with, whatever a test did: the containers are stopped and
/// removed, the directories removed, and the bound taken off the project. It is declared
/// after the subject, so it drops first.
struct Bound {
    quota: XfsQuota,
    runtime: Shared,
    run_dir: PathBuf,
    lease_dir: PathBuf,
    neighbour: PathBuf,
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
        let _ = fs::remove_file(&self.neighbour);
        let _ = fs::remove_dir_all(&self.lease_dir);
        let _ = fs::remove_dir_all(&self.run_dir);
        let _ = self.quota.clear(&self.run_dir, self.project);
    }
}

/// How a run is set up around its job.
#[derive(Clone, Copy)]
struct Around {
    /// Whether the run's directory and its lease directory are put under the run's project, as
    /// the lifecycle does; otherwise neither is, and what the job writes is in project 0.
    bound: bool,
    /// What a neighbour writes to the volume, outside the run, while the run is held.
    neighbour: u64,
}

/// A run as the lifecycle sets it up, alone on the volume.
const BOUND: Around = Around {
    bound: true,
    neighbour: 0,
};

/// What a job that wrote to one place left behind.
struct Written {
    exit: i32,
    stderr: String,
    used: u64,
    /// The bytes of the run's directories that are not in its project ([`escaped`]).
    escaped: u64,
    /// The bytes the neighbour's file holds on the volume (0 with no neighbour).
    neighbour: u64,
    /// Whether the lease is in the image store (it must not be: only the scratch root).
    lease_in_store: bool,
}

/// Runs a job that writes `mib` MiB to `target` (`/scratch`, `/work` or `/root`), with the
/// run's directories set up as `around` says under one project of `slot` limited to
/// [`LIMIT`], waits for it to end and reads what it left while the container is still held.
fn write_to(target: &str, mib: u64, slot: u32, around: Around) -> Written {
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
    let neighbour = subject
        .runs()
        .parent()
        .expect("the runs are in the subject's directory")
        .join("neighbour");
    let bound = Bound {
        quota,
        runtime: subject.runtime(),
        run_dir: spec.scratch.clone(),
        lease_dir: lease_dir.clone(),
        neighbour: neighbour.clone(),
        project,
    };
    // The lifecycle's order: both directories empty, both bound, then the container.
    fs::create_dir(&lease_dir).expect("make the lease directory");
    if around.bound {
        for dir in [&spec.scratch, &lease_dir] {
            bound
                .quota
                .apply(dir, project, LIMIT, INODES)
                .unwrap_or_else(|e| panic!("bound {}: {e}", dir.display()));
        }
    }

    let id = subject
        .runtime()
        .borrow_mut()
        .create_and_start(&spec)
        .expect("create and start");
    if around.neighbour > 0 {
        fill(&neighbour, around.neighbour);
    }
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
        escaped: escaped(&volume, &[&spec.scratch, &lease_dir], project),
        neighbour: fs::symlink_metadata(&neighbour).map_or(0, |m| m.blocks() * 512),
        lease_in_store: subject.lease_dir(&lease_name).exists(),
    };
    drop(bound);
    written
}

/// Asserts, for one place, that a job writing far past the limit is refused: it exits with the
/// failure of its `dd`, says the disk is full, what it wrote is held to the limit, and none of
/// it escaped the project. It catches a lease directory outside the project, where `/work`
/// and `/root` are written until the volume is full.
fn refused(target: &str, slot: u32, around: Around) -> Written {
    let w = write_to(target, ATTEMPT_MIB, slot, around);
    assert_eq!(w.exit, 7, "the write was not refused: {}", w.stderr);
    assert!(
        w.stderr.contains("No space left on device"),
        "not refused as a full disk: {}",
        w.stderr
    );
    assert!(w.used > LIMIT / 2, "the bytes were not counted: {}", w.used);
    assert!(w.used <= LIMIT, "the limit was passed: {}", w.used);
    assert_eq!(w.escaped, 0, "bytes escaped the run's project");
    assert!(!w.lease_in_store, "the lease is in the image store");
    w
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_work_is_refused() {
    refused("/work", 20, BOUND);
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_root_is_refused() {
    refused("/root", 21, BOUND);
}

#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_past_the_quota_into_scratch_is_refused() {
    refused("/scratch", 22, BOUND);
}

/// Asserts that room the volume loses outside the run is not taken for an escape: while a
/// refused job's run is held, a neighbour writes [`NEIGHBOUR`] bytes, as much as the job tries
/// to write, to the volume outside the run's directories, and its file holds at least that
/// much of the volume, while the run still passes every check of [`refused`], with nothing
/// escaped. It catches an escape check that reads the volume's free space, which a neighbour's
/// write reaches whatever bound it sets for a whole escape.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn room_the_volume_loses_outside_the_run_is_not_an_escape() {
    let w = refused(
        "/scratch",
        23,
        Around {
            bound: true,
            neighbour: NEIGHBOUR,
        },
    );
    assert!(
        w.neighbour >= NEIGHBOUR,
        "the neighbour did not take room on the volume: {}",
        w.neighbour
    );
}

/// Asserts that a write outside the run's project is measured as escaped, for each place: with
/// neither of the run's directories bound, a job that writes 2 MiB to one of them ends well,
/// the project holds nothing, and at least the 2 MiB are found outside it. It catches an
/// escape check that does not look where the job writes, which would pass a run whose bytes
/// no quota counts.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_write_outside_the_runs_project_is_an_escape() {
    let unbound = Around {
        bound: false,
        neighbour: 0,
    };
    for (slot, target) in [(50, "/scratch"), (51, "/work"), (52, "/root")] {
        let w = write_to(target, 2, slot, unbound);
        assert_eq!(w.exit, 0, "{target}: {}", w.stderr);
        assert_eq!(w.used, 0, "{target}: the project holds {} bytes", w.used);
        assert!(
            w.escaped >= 2 << 20,
            "{target}: escaped {} bytes",
            w.escaped
        );
    }
}

/// Asserts that the project's usage counts each of the three places: a job that writes 2 MiB
/// to only one of them, and ends well, leaves at least that much in the project and nothing
/// outside it. It catches a usage that reads `/scratch` alone, which the cleanup would believe
/// empty while `/work` or `/root` still held a run's bytes.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn the_usage_of_a_project_counts_scratch_work_and_root() {
    for (slot, target) in [(30, "/scratch"), (31, "/work"), (32, "/root")] {
        let w = write_to(target, 2, slot, BOUND);
        assert_eq!(w.exit, 0, "{target}: {}", w.stderr);
        assert!(w.used >= 2 << 20, "{target}: usage {} bytes", w.used);
        assert!(w.used <= LIMIT, "{target}: usage {} bytes", w.used);
        assert_eq!(w.escaped, 0, "{target}: bytes escaped the run's project");
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
        neighbour: subject.runs().join("no-neighbour"),
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
