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
/// brings it back, and that one is a note (logged before the checks pass), frees the slot
/// and does not taint the host; and that a job's process left running taints the host and
/// keeps the slot. It catches a baseline that is never retaken after a clean job, a taint
/// for a mount, and a leftover process that frees its slot.
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
            "finding r2 new-mount",
            "checks passed r2",
            "freed r2"
        ]
    );
    assert_eq!(h.life.free().cpu_millis, 4000, "the slot is free");
    assert!(!h.life.is_tainted(), "a mount is the host's own doing");
    assert_eq!(
        h.life.findings()[0].message,
        "/mnt/data (ext4) from /dev/sdb1"
    );

    h.start(job("r3"));
    host.start_process(process(
        300,
        100,
        JOB_UID,
        900,
        "0::/machine.slice/libpod-9e1.scope",
    ));
    h.fake.exit("r3", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r3",
            "container removed r3",
            "scratch removed r3",
            "tainted r3 residue",
            "finding r3 process-as-job-user"
        ]
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is kept");
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.findings()[1].message,
        "pid 300 (stray, state S, cgroup 0::/machine.slice/libpod-9e1.scope, owner 65534) is running after job r3"
    );
}

/// Asserts that a job whose check found a leftover does not move the baseline's processes of
/// the job users: another job that was running beside it, and ends cleanly, is compared with
/// the same baseline and finds the same process, and the agent, tainted, lets it finish. It
/// catches a baseline moved over a leftover, which would let the second job call the host
/// clean.
#[test]
fn a_dirty_job_leaves_the_baseline_where_it_was() {
    let host = FakeHost::quiet();
    let mut h = checked("dirty", &host);
    h.start(job("r1"));
    h.start(job("r2"));
    host.start_process(process(
        300,
        100,
        JOB_UID,
        900,
        "0::/machine.slice/libpod-9e1.scope",
    ));
    h.fake.exit("r1", 0);
    h.life.step();
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "finding r2 process-as-job-user");
    assert_eq!(h.life.free().cpu_millis, 2000, "neither slot came back");
    assert_eq!(h.life.findings().len(), 2);
}

/// Asserts, with the real checks over the in-memory host, that a process of the job user a
/// job left taints the host and that the host clears itself once the process is gone: while
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
        100,
        JOB_UID,
        900,
        "0::/machine.slice/libpod-9e1.scope",
    ));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert_eq!(
        h.life.pending_taint().unwrap().check.unwrap(),
        "process-as-job-user"
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
/// it started: a module an operator loads between jobs is not the next job's, and is not
/// noted. A baseline kept from the last check would name it in a `kernel-module-changed` note
/// for something that happened while no job ran.
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
    assert!(h.life.findings().is_empty(), "nothing was noted");
}

/// Asserts that a job that starts beside another does not move the baseline: a module
/// loaded while the first job runs is still noted when it ends, though a second job started
/// after it, and the note taints nothing and holds nothing. It catches a baseline retaken at
/// every start, which would let a running job's change go unnoted, and a taint for a module.
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
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "finding r1 kernel-module-changed",
            "checks passed r1",
            "freed r1"
        ]
    );
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.findings()[0].message, "loaded evil");
}

/// Asserts that a host whose process table cannot be read when a job starts on it idle keeps
/// the baseline it had, and that the check after the job finds it unreadable and taints the
/// host as unchecked. It catches a baseline replaced by an empty or partial reading, which
/// would hide whatever the job then left.
#[test]
fn an_unreadable_host_at_an_idle_start_is_found_by_the_check() {
    let host = FakeHost::quiet();
    let mut h = checked("idle-unreadable", &host);
    host.fail(input::PROCESSES);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events.last().unwrap(),
        "finding r1 processes-unreadable",
        "{events:?}"
    );
    assert!(h.life.is_tainted());
}

/// Asserts that a module list that cannot be read is a note after the job, not a taint: the
/// job's slot is freed and the host is not tainted, and the note names the reason. It catches
/// a taint for an input of a check that no longer taints.
#[test]
fn an_unreadable_module_list_is_a_note_and_taints_nothing() {
    let host = FakeHost::quiet();
    let mut h = checked("idle-modules", &host);
    host.fail(input::MODULES);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert!(
        events.contains(&"finding r1 modules-unreadable".to_owned()),
        "{events:?}"
    );
    assert_eq!(events.last().unwrap(), "freed r1", "{events:?}");
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.findings()[0].message, "modules: cannot be read");
}

/// An agent that has just started over `host` and found a marker of a residue of the check
/// `check` in its state root: the checks took their baseline now, so the baseline holds
/// whatever is on the host. The report of the taint is acknowledged.
fn restarted_with_marker(name: &str, host: &FakeHost, check: &str) -> Harness {
    let mut h = Harness::new(name);
    let taint = js_agent_wire::Taint {
        reason: "residue".to_owned(),
        run_id: Some("r1".to_owned()),
        check: Some(check.to_owned()),
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
/// restored `process-as-job-user` taint without the baseline: a process of the job user,
/// which is in the restart baseline, keeps the host tainted for as many waits as pass; when it
/// is gone, two clean re-checks report the clear and the acknowledgement ends the taint. It
/// catches a restart that lets the restart baseline hide the process the tainting job left,
/// and a restored taint that never clears.
#[test]
fn a_restored_taint_clears_only_when_the_process_in_the_baseline_is_gone() {
    let host = FakeHost::quiet();
    host.start_process(process(
        300,
        100,
        JOB_UID,
        900,
        "0::/machine.slice/libpod-9e1.scope",
    ));
    let mut h = restarted_with_marker("restored-baseline", &host, "process-as-job-user");
    wait_rechecks(&mut h, 5);
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);

    host.end_process(300);
    wait_rechecks(&mut h, 1);
    assert_eq!(h.life.pending_taint_clear(), None, "one clean check");
    wait_rechecks(&mut h, 1);
    assert!(h.life.pending_taint_clear().is_some());
    let events = h
        .life
        .apply(&reply_json(&json!({ "taint_cleared_ack": true })));
    assert_eq!(names(&events), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that a marker written by an earlier version for a check that now only notes
/// (`daemonised-child`, `new-mount`, `user-changed`, `group-changed`, `kernel-module-changed`,
/// `watch-dir-changed`) clears after two clean re-checks even though a daemonised process of
/// the host, in the restart baseline, is on the host: the re-check judges only a process of
/// the job users' range. It catches a taint of a demoted check that holds a host for a
/// change that was always the host's own.
#[test]
fn a_marker_of_a_check_that_now_only_notes_clears_at_the_next_clean_recheck() {
    for check in [
        "daemonised-child",
        "new-mount",
        "user-changed",
        "group-changed",
        "kernel-module-changed",
        "watch-dir-changed",
    ] {
        let host = FakeHost::quiet();
        host.start_process(process(
            301,
            1,
            0,
            900,
            "0::/system.slice/agent.service/leftover",
        ));
        let mut h = restarted_with_marker(&format!("legacy-{check}"), &host, check);
        wait_rechecks(&mut h, 2);
        assert!(h.life.pending_taint_clear().is_some(), "{check}");
        let events = h
            .life
            .apply(&reply_json(&json!({ "taint_cleared_ack": true })));
        assert_eq!(names(&events), ["finding - taint-cleared"], "{check}");
        assert!(!h.life.is_tainted(), "{check}");
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
    let mut h = restarted_with_marker("restored-quiet", &host, "process-as-job-user");
    wait_rechecks(&mut h, 2);
    assert!(h.life.pending_taint_clear().is_some());
}
