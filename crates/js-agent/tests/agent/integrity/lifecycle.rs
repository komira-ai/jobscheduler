//! The checks as the lifecycle runs them: the real [`Integrity`] over the in-memory host,
//! after the container and scratch of each job are gone.

use js_agent::integrity::{FakeHost, Integrity, input};

use serde_json::json;

use super::{JOB_UID, mount, process, started};
use crate::lifecycle::{Harness, Options, cleanup, job, names, reply_json};

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

/// Asserts, with the real checks over the in-memory host, that a daemonised process a job
/// left taints the host and that the host clears itself once the process is gone: while
/// the process is there the re-checks find it again and again and the host stays tainted;
/// when it ends, two clean re-checks a wait apart report the clear, and the server's
/// acknowledgement ends the taint, frees the blocked run and its slot, and the next job
/// runs. It catches re-checks that do not look at the host, and a clear that does not need
/// the process to be gone.
#[test]
fn a_taint_for_a_process_clears_when_the_process_is_gone() {
    let host = FakeHost::quiet();
    let mut h = checked("clears-when-gone", &host);
    h.start(job("r1"));
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.pending_taint().unwrap().check.unwrap(),
        "daemonised-child"
    );
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));

    h.life.step();
    for _ in 0..4 {
        h.clock.advance(60_000);
        h.life.step();
    }
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.pending_taint_clear(),
        None,
        "the process is still there"
    );

    host.end_process(300);
    h.clock.advance(60_000);
    h.life.step();
    assert_eq!(h.life.pending_taint_clear(), None, "one clean check");
    h.clock.advance(60_000);
    h.life.step();
    assert!(h.life.pending_taint_clear().is_some());
    let events = h
        .life
        .apply(&reply_json(&json!({ "taint_cleared_ack": true })));
    assert_eq!(names(&events), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert_eq!(names(&h.start(job("r2"))), ["started r2"]);
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

/// Asserts that a job that starts on an idle host is compared with the host as it was when
/// it started: a module an operator loads between jobs is not the next job's. A baseline
/// kept from the last check would call it `kernel-module-changed` and taint the host for
/// something that happened while no job ran.
#[test]
fn a_job_on_an_idle_host_starts_from_the_host_as_it_is() {
    let host = FakeHost::quiet();
    let mut h = checked("idle", &host);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    host.load_module("zram");
    h.start(job("r2"));
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "freed r2", "{events:?}");
    assert!(!h.life.is_tainted());
}

/// Asserts that a job that starts beside another does not move the baseline: a module
/// loaded while the first job runs is still found when it ends, though a second job started
/// after it. It catches a baseline retaken at every start, which would let a running job's
/// change pass.
#[test]
fn a_job_that_starts_beside_another_keeps_the_baseline() {
    let host = FakeHost::quiet();
    let mut h = checked("busy", &host);
    h.start(job("r1"));
    host.load_module("evil");
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events.last().unwrap(),
        "finding r1 kernel-module-changed",
        "{events:?}"
    );
    assert!(h.life.is_tainted());
}

/// Asserts that a host that cannot be read when a job starts on it idle keeps the baseline
/// it had, and that the check after the job finds it unreadable and taints the host. It
/// catches a baseline replaced by an empty or partial reading, which would hide whatever
/// the job then left.
#[test]
fn an_unreadable_host_at_an_idle_start_is_found_by_the_check() {
    let host = FakeHost::quiet();
    let mut h = checked("idle-unreadable", &host);
    host.fail(input::MODULES);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events.last().unwrap(),
        "finding r1 modules-unreadable",
        "{events:?}"
    );
    assert!(h.life.is_tainted());
}

/// An agent that has just started over `host` and found a marker of a residue of a process in
/// its state root: the checks took their baseline now, so the baseline holds whatever is on
/// the host. The report of the taint is acknowledged.
fn restarted_with_marker(name: &str, host: &FakeHost) -> Harness {
    let mut h = Harness::new(name);
    let taint = js_agent_wire::Taint {
        reason: "residue".to_owned(),
        run_id: Some("r1".to_owned()),
        check: Some("daemonised-child".to_owned()),
    };
    h.config.state.write(&taint).unwrap();
    h.restart_with(Box::new(started(host, vec![]))).unwrap();
    assert!(h.life.is_tainted());
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));
    h.life.step();
    h
}

/// Steps `waits` re-check intervals of the default wait.
fn wait_rechecks(h: &mut Harness, waits: u32) {
    for _ in 0..waits {
        h.clock.advance(60_000);
        h.life.step();
    }
}

/// Asserts, with the real checks over the in-memory host, that a restarted agent re-checks a
/// restored residue taint without the baseline: a process of the job user, and one with
/// parent `init` in the agent's own group, are in the restart baseline and keep the host
/// tainted for as many waits as pass; when they are gone, two clean re-checks report the
/// clear and the acknowledgement ends the taint. It catches a restart that lets the restart
/// baseline hide the process the tainting job left, and a restored taint that never clears.
#[test]
fn a_restored_taint_clears_only_when_the_process_in_the_baseline_is_gone() {
    for (pid, ppid, uid, cgroup) in [
        (300, 100, JOB_UID, "0::/machine.slice/libpod-9e1.scope"),
        (301, 1, 0, "0::/system.slice/agent.service/leftover"),
    ] {
        let host = FakeHost::quiet();
        host.start_process(process(pid, ppid, uid, 900, cgroup));
        let mut h = restarted_with_marker("restored-baseline", &host);
        wait_rechecks(&mut h, 5);
        assert!(h.life.is_tainted(), "{cgroup}");
        assert_eq!(h.life.pending_taint_clear(), None, "{cgroup}");

        host.end_process(pid);
        wait_rechecks(&mut h, 1);
        assert_eq!(
            h.life.pending_taint_clear(),
            None,
            "{cgroup}: one clean check"
        );
        wait_rechecks(&mut h, 1);
        assert!(h.life.pending_taint_clear().is_some(), "{cgroup}");
        let events = h
            .life
            .apply(&reply_json(&json!({ "taint_cleared_ack": true })));
        assert_eq!(names(&events), ["finding - taint-cleared"], "{cgroup}");
        assert!(!h.life.is_tainted(), "{cgroup}");
    }
}

/// Asserts that a restored residue taint on a host with nothing of a job on it clears after
/// two re-checks, though the host holds the agent itself (parent `init`, in its own group)
/// and transient processes of the host with parent `init` in a scope of the system slice and
/// in `init.scope`, all in the restart baseline. It catches a baseline-free rule that counts
/// the agent or the host's own processes, which would hold every restarted host tainted.
#[test]
fn a_restored_taint_clears_on_a_host_with_only_its_own_processes() {
    let host = FakeHost::quiet();
    host.start_process(process(300, 1, 0, 900, "0::/system.slice/foo.scope"));
    host.start_process(process(301, 1, 0, 901, "0::/init.scope"));
    let mut h = restarted_with_marker("restored-quiet", &host);
    wait_rechecks(&mut h, 2);
    assert!(h.life.pending_taint_clear().is_some());
}
