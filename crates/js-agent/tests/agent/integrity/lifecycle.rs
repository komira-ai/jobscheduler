//! The checks as the lifecycle runs them: the real [`Integrity`] over the in-memory host,
//! after the container and scratch of each job are gone.

use js_agent::integrity::{FakeHost, Integrity};

use super::{JOB_UID, mount, process, started};
use crate::lifecycle::{Harness, Options, cleanup, job, names};

/// The harness with `host` checked after every job.
fn checked(name: &str, host: &FakeHost) -> Harness {
    let checks: Integrity = started(host, vec![]);
    Harness::with(
        name,
        Options {
            hook: Some(Box::new(checks)),
            ..Options::default()
        },
    )
}

/// Asserts that a job that leaves nothing behind frees its slot and moves the baseline
/// forward: a mount that went away during the first job is a new mount when the second job
/// brings it back, and that one is a finding, taints the host and keeps the slot. It
/// catches a baseline that is never retaken after a clean job, and a clean job that does
/// not free its slot.
#[test]
fn clean_job_frees_slot_and_rebaselines() {
    let host = FakeHost::quiet();
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    let mut h = checked("rebaseline", &host);
    h.start(job("r1"));
    host.unmount("/mnt/data");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert_eq!(h.life.free().cpu_millis, 4000, "the slot is free");
    assert!(!h.life.is_tainted());

    h.start(job("r2"));
    host.mount(mount("/mnt/data", "/dev/sdb1"));
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r2",
            "container removed r2",
            "scratch removed r2",
            "tainted r2 residue",
            "finding r2 new-mount"
        ]
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.findings()[0].message,
        "new-mount: /mnt/data (ext4) from /dev/sdb1"
    );
}

/// Asserts that a job whose check found something does not move the baseline: another job
/// that was running beside it, and ends cleanly, is compared with the same baseline and
/// finds the same module, and the agent, tainted, lets it finish. It catches a baseline
/// moved over a finding, which would let the second job call the changed host clean.
#[test]
fn a_dirty_job_leaves_the_baseline_where_it_was() {
    let host = FakeHost::quiet();
    let mut h = checked("dirty", &host);
    h.start(job("r1"));
    h.start(job("r2"));
    host.load_module("evil");
    h.fake.exit("r1", 0);
    h.life.step();
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "finding r2 kernel-module-changed");
    assert_eq!(h.life.free().cpu_millis, 2000, "neither slot came back");
    assert_eq!(h.life.findings().len(), 2);
}

/// Asserts that the checks are told every container that is still on the host beside the
/// finished job: one that runs, one that exited and could not be removed yet, and one the
/// agent found and could not remove. It catches a list that leaves one of them out, which
/// would blame the job for that container's processes.
#[test]
fn the_checks_are_told_every_container_beside_the_job() {
    let mut h = Harness::new("live-list");
    h.start(job("r3"));
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.leak(
        &[
            ("jobscheduler.dev/run-id", "../x"),
            ("jobscheduler.dev/isolation", "contained"),
        ],
        0,
    );
    h.fake.fail("remove", 1);
    h.restart().unwrap();
    h.fake.fail("remove", 1);
    h.fake.exit("r3", 0);
    h.fake.exit("r1", 0);
    h.log.borrow_mut().clear();
    h.life.step();
    let log = h.log.borrow().clone();
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("live beside"))
            .collect::<Vec<_>>(),
        ["live beside r1: c1,c3,leaked-4"]
    );
}

/// Asserts that the lifecycle tells the checks which containers of other jobs are still on
/// the host, and that a job is not blamed for them: the first job ends beside a second one
/// whose process runs as the container user, and passes; the second job's own check, once
/// its container is gone and its process lingers, finds the process. It catches a lifecycle
/// that passes no neighbours (every concurrent job would taint the host), and one that lets
/// a neighbour's leftover process pass.
#[test]
fn a_neighbours_container_is_not_the_jobs_own() {
    let host = FakeHost::quiet();
    let mut h = checked("neighbour", &host);
    h.start(job("r1"));
    h.start(job("r2"));
    host.start_process(process(700, 600, JOB_UID, 2000, "0::/docker/c2"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "freed r1", "{events:?}");
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events.last().unwrap(),
        "finding r2 process-as-job-user",
        "its process outlived its container: {events:?}"
    );
}
