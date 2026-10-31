//! The directory a runtime keeps beside a run's own (the crun runtime's lease: a container's
//! `/work` overlay and home), over the fake runtime and the in-memory quota: the lifecycle
//! makes it empty, puts it under the run's project, and removes it with the run's directory
//! and at the start-up sweep. What the kernel then refuses is `tests/crun_quota`'s.

use std::fs;
use std::path::PathBuf;

use js_agent::quota::Call;
use js_agent_wire::Outcome;

use super::quota::{DEFAULT_BYTES, INODES, applies, clears, project, with_quota};
use super::{Harness, cleanup, job, names};

/// Makes the fake runtime keep a directory for each run in the scratch root.
fn keep_lease_dirs(h: &Harness) {
    let root = h.scratch_of("any").parent().unwrap().to_owned();
    h.fake.keep_lease_dirs_in(&root);
}

/// The runtime's directory for `run`.
fn lease_of(h: &Harness, run: &str) -> PathBuf {
    h.scratch_of(run)
        .with_file_name(format!(".lease-fake-{run}"))
}

/// Asserts that a run's lease directory gets the run's project and limits: the lifecycle
/// makes it empty in the scratch root, and both directories are bound before the container
/// is created (the run's first, then the lease's, each empty). It catches a bound on the run's
/// directory alone, which leaves `/work` and `/root` to fill the disk, and a bound set after
/// the container is made, when files may be written already.
#[test]
fn a_lease_directory_is_bound_with_the_run_before_the_container_is_made() {
    let (mut h, spy) = with_quota("lease-bind");
    keep_lease_dirs(&h);
    h.start(job("r1"));
    assert_eq!(
        spy.seen(),
        [
            "apply r1 empty=true ok=true",
            "apply .lease-fake-r1 empty=true ok=true"
        ]
    );
    let bound: Vec<_> = applies(&spy)
        .into_iter()
        .map(|call| match call {
            Call::Apply {
                dir,
                project,
                block_limit_bytes,
                inode_limit,
            } => (
                dir.file_name().unwrap().to_string_lossy().into_owned(),
                project,
                block_limit_bytes,
                inode_limit,
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        bound,
        [
            ("r1".to_owned(), project(0), DEFAULT_BYTES, INODES),
            (
                ".lease-fake-r1".to_owned(),
                project(0),
                DEFAULT_BYTES,
                INODES
            ),
        ]
    );
    assert!(lease_of(&h, "r1").is_dir());
    assert!(
        h.log
            .borrow()
            .iter()
            .any(|line| line == "create r1 [quota set]"),
        "{:?}",
        h.log.borrow()
    );
}

/// Asserts that the lease directory goes with the run: after the run ends its cleanup removes
/// the directory, and the project is cleared only once both trees are gone (the scratch root
/// lists neither), once. It catches a lease directory left to fill the disk after its run, and
/// a project cleared while its lease still holds files.
#[test]
fn the_lease_directory_is_removed_with_the_run_before_the_project_is_cleared() {
    let (mut h, spy) = with_quota("lease-remove");
    keep_lease_dirs(&h);
    h.start(job("r1"));
    assert!(lease_of(&h, "r1").is_dir(), "the lease directory was made");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert!(!lease_of(&h, "r1").exists());
    assert_eq!(clears(&spy).len(), 1);
    assert_eq!(
        spy.seen().last().map(String::as_str),
        Some("clear r1 listed=[]")
    );
}

/// Asserts that a run with no scratch quota still gets its lease directory made in the scratch
/// root and removed with the run. It catches a layout that depends on the quota being on.
#[test]
fn the_lease_directory_is_made_and_removed_without_a_quota() {
    let mut h = Harness::new("lease-no-quota");
    keep_lease_dirs(&h);
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    assert!(lease_of(&h, "r1").is_dir());
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!lease_of(&h, "r1").exists());
}

/// Asserts the start-up sweep: a leftover lease directory of a run that no longer exists is
/// removed with what is in it, while the directory of a run the new process adopts and that
/// run's lease directory stay. It catches a sweep that leaves a dead run's lease to hold disk,
/// and one that removes the lease of a live run.
#[test]
fn the_sweep_removes_a_leftover_lease_directory_and_keeps_an_adopted_runs() {
    let (mut h, _spy) = with_quota("lease-sweep");
    keep_lease_dirs(&h);
    h.start(job("r1"));
    let gone = lease_of(&h, "gone");
    fs::create_dir(&gone).unwrap();
    fs::write(gone.join("upper"), b"left behind").unwrap();
    assert_eq!(names(&h.restart().unwrap()), ["adopted r1"]);
    assert!(!gone.exists(), "the leftover lease directory is removed");
    assert!(
        h.scratch_of("r1").is_dir(),
        "the adopted run's directory stays"
    );
    assert!(
        lease_of(&h, "r1").is_dir(),
        "and so does its lease directory"
    );
}

/// Asserts that a runtime directory that cannot be made refuses the run and creates nothing:
/// the run ends `failed_to_start` with `scratch-failed`, no container exists and nothing was
/// bound. It catches a run that starts without its lease directory bound.
#[test]
fn a_lease_directory_that_cannot_be_made_refuses_the_run() {
    let (mut h, spy) = with_quota("lease-unnamable");
    h.fake.keep_unnamable_lease_dirs();
    assert_eq!(
        names(&h.start(job("r1"))),
        ["finding r1 scratch-failed", "result r1"]
    );
    assert!(h.fake.created().is_empty(), "no container");
    assert!(spy.seen().is_empty(), "nothing was bound");
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("scratch-failed"));
}
