//! The settle wait of the process and mount checks: a process or mount the baseline did not
//! have is a job's leftover only when it is still there after the wait, and the host the
//! checks compare is the one after the wait. The host is the in-memory fake, and the wait is
//! a call it records, not time that passes.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use js_agent::integrity::{
    FakeHost, HostView, Integrity, Mount, ProcHost, Process, Watched, code, input,
};
use js_agent::lifecycle::CheckFinding;

use super::{JOB_USERS, after, mount, process, process_named, said, started};

/// The wait of these tests.
const WAIT: Duration = Duration::from_millis(300);

/// Asserts that a process whose parent is `init` that is there at the first look and gone
/// after the wait is not a finding (the checks asked for the wait once), and that the same
/// process, still there after the wait, is the `daemonised-child` it always was. It catches
/// a host's own process that lives a moment, such as the service manager's fork of a timer's
/// unit, tainting a node, and a wait that lets a real leftover go.
#[test]
fn a_process_that_is_gone_after_the_wait_is_not_a_leftover_and_one_that_stays_is() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    host.at_settle(|host| host.end_process(300));
    assert!(after(&mut checks, "r1").is_ok());
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::DAEMONISED_CHILD,
            "daemonised-child: pid 300 (stray, state S, cgroup 0::/system.slice/agent.service/left) was left behind by job r1 (parent init)".to_owned()
        )
    );
    assert_eq!(host.settled(), [WAIT]);
}

/// Asserts that a process of the job's user that is there at both looks is still found, and
/// that one that is gone at the second look is not. It catches the wait applied to one of
/// the two process checks only.
#[test]
fn the_wait_applies_to_a_process_of_the_job_user_too() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(301, 100, super::JOB_UID, 901, "0::/"));
    host.at_settle(|host| host.end_process(301));
    assert!(after(&mut checks, "r1").is_ok());

    host.start_process(process(302, 100, super::JOB_UID, 902, "0::/"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
}

/// Asserts that a process that starts during the wait is not judged by this check and is not
/// part of the baseline it takes, so the next check judges it, with a wait of its own: it is
/// found there when it is still there. It catches a process that arrives during the wait
/// hidden in the baseline for good.
#[test]
fn a_process_that_starts_during_the_wait_is_judged_by_the_next_check() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    host.at_settle(|host| {
        host.end_process(300);
        host.start_process(process(
            303,
            1,
            0,
            903,
            "0::/system.slice/agent.service/left",
        ));
    });
    assert!(after(&mut checks, "r1").is_ok());

    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::DAEMONISED_CHILD,
            "daemonised-child: pid 303 (stray, state S, cgroup 0::/system.slice/agent.service/left) was left behind by job r2 (parent init)".to_owned()
        )
    );
    assert_eq!(host.settled(), [WAIT, WAIT]);
}

/// Asserts that a process table that cannot be read at the second look is a check that could
/// not run, not a pass. It catches a failed second look read as an empty host.
#[test]
fn a_second_look_that_cannot_read_the_processes_is_unchecked() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    host.at_settle(|host| host.fail(input::PROCESSES));
    let found = after(&mut checks, "r1").unwrap_err();
    assert!(matches!(found, CheckFinding::Unchecked { .. }), "{found:?}");
    assert_eq!(found.code(), code::PROCESSES_UNREADABLE);
}

/// Asserts that the checks wait only when a leftover is what they would find: a clean host
/// and a host whose only change is a user added do not wait, a new mount that stays waits
/// once and is still a `new-mount`, and checks that were given no wait look once even for a
/// stray process or a new mount. It catches a wait on every job, a wait on a finding that
/// the wait cannot change, and a wait where none was asked for.
#[test]
fn the_checks_wait_only_for_a_leftover_and_only_when_given_a_wait() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    assert!(after(&mut checks, "r1").is_ok());
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::USER_CHANGED);
    assert!(host.settled().is_empty());

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(mount("/mnt/x", "/dev/sdb"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::NEW_MOUNT);
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    assert!(after(&mut checks, "r1").is_err());
    host.mount(mount("/mnt/x", "/dev/sdb"));
    // It would be gone at a wait, which a check given no wait never takes: it is reported.
    host.at_settle(|host| host.unmount("/mnt/x"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
    assert!(
        found
            .detail()
            .contains("new-mount: /mnt/x (ext4) from /dev/sdb"),
        "{found:?}"
    );
    assert!(host.settled().is_empty());
}

/// A FUSE mount a process of the host makes below a build directory and holds for a moment.
fn fuse_mount(name: &str) -> Mount {
    Mount {
        root: "/".to_owned(),
        point: format!("/srv/build/tmp/{name}/mnt"),
        fstype: "fuse.testfs".to_owned(),
        source: "testfs".to_owned(),
    }
}

/// Asserts that a new mount that is there at the first look and gone after the wait is not a
/// finding (the checks asked for the wait once and the host is not tainted), and that the
/// same mount, still there after the wait, is the `new-mount` it always was, named whole. It
/// catches a mount the host holds for a moment as a job ends (a FUSE mount of a process
/// on the host, unmounted a few hundred milliseconds after the check) tainting the host for good,
/// and a wait that lets a mount that outlives the job go.
#[test]
fn a_mount_that_is_gone_after_the_wait_is_not_a_leftover_and_one_that_stays_is() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(fuse_mount("a-1"));
    host.mount(fuse_mount("b-2"));
    host.at_settle(|host| {
        host.unmount("/srv/build/tmp/a-1/mnt");
        host.unmount("/srv/build/tmp/b-2/mnt");
    });
    assert!(after(&mut checks, "r1").is_ok());
    assert_eq!(host.settled(), [WAIT]);
    assert!(after(&mut checks, "r2").is_ok());
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(fuse_mount("a-1"));
    host.mount(fuse_mount("b-2"));
    host.at_settle(|host| host.unmount("/srv/build/tmp/a-1/mnt"));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::NEW_MOUNT,
            "new-mount: /srv/build/tmp/b-2/mnt (fuse.testfs) from testfs".to_owned()
        )
    );
    assert_eq!(host.settled(), [WAIT]);
}

/// Asserts that a mount of the finished job's own file system tree (below its directory of
/// the runtime's mount root, which no held job names) that is still unwinding at the first
/// look and gone after the wait is not a finding, and that the same mount, still there after
/// the wait, is a `new-mount`. It catches the runtime's unmount racing the check, and a wait
/// that hides a tree mount that outlives its job.
#[test]
fn a_mount_of_the_jobs_own_tree_is_judged_after_the_wait() {
    let tree = Mount {
        root: "/".to_owned(),
        point: "/run/js-agent/mounts/js-a-r1/rootfs".to_owned(),
        fstype: "overlay".to_owned(),
        source: "overlay".to_owned(),
    };
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(tree.clone());
    host.at_settle(|host| host.unmount("/run/js-agent/mounts/js-a-r1/rootfs"));
    assert!(after(&mut checks, "r1").is_ok());

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(tree);
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::NEW_MOUNT,
            "new-mount: /run/js-agent/mounts/js-a-r1/rootfs (overlay) from overlay".to_owned()
        )
    );
}

/// Asserts that a mount made during the wait is not judged by this check and is not part of
/// the baseline it takes, so the next check judges it, with a wait of its own, and finds it
/// when it is still there. It catches a mount that arrives during the wait hidden in the
/// baseline for good.
#[test]
fn a_mount_made_during_the_wait_is_judged_by_the_next_check() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(fuse_mount("c-3"));
    host.at_settle(|host| {
        host.unmount("/srv/build/tmp/c-3/mnt");
        host.mount(mount("/mnt/late", "/dev/sdc"));
    });
    assert!(after(&mut checks, "r1").is_ok());

    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::NEW_MOUNT,
            "new-mount: /mnt/late (ext4) from /dev/sdc".to_owned()
        )
    );
    assert_eq!(host.settled(), [WAIT, WAIT]);
}

/// Asserts that a mount of the baseline that is gone at the first look and back after the
/// wait (a host service that remounts its file system as a job ends) stays in the baseline
/// the check takes, so the next check, with nothing changed, passes without a wait. It
/// catches the second look dropping a remounted baseline mount as one that arrived during
/// the wait, which would taint the host at the next job's end.
#[test]
fn a_baseline_mount_remounted_during_the_wait_stays_in_the_baseline() {
    let host = FakeHost::quiet();
    host.mount(mount("/run/tierstore/fs", "tierstore"));
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.unmount("/run/tierstore/fs");
    host.mount(fuse_mount("d-4"));
    host.at_settle(|host| {
        host.unmount("/srv/build/tmp/d-4/mnt");
        host.mount(mount("/run/tierstore/fs", "tierstore"));
    });
    assert!(after(&mut checks, "r1").is_ok());
    assert!(after(&mut checks, "r2").is_ok());
    assert_eq!(host.settled(), [WAIT]);
}

/// Asserts that a mount table that cannot be read at the second look is a check that could
/// not run, not a pass. It catches a failed second look read as a host with no new mount.
#[test]
fn a_second_look_that_cannot_read_the_mounts_is_unchecked() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(fuse_mount("e-5"));
    host.at_settle(|host| host.fail(input::MOUNTS));
    let found = after(&mut checks, "r1").unwrap_err();
    assert!(matches!(found, CheckFinding::Unchecked { .. }), "{found:?}");
    assert_eq!(found.code(), code::MOUNTS_UNREADABLE);
}

/// Asserts that the machine's own host view waits when asked, and that the in-memory host
/// records the wait: the real wait is a call, not a check on time. It catches a view whose
/// wait is a no-op on the machine.
#[test]
fn the_machines_view_waits_when_asked() {
    ProcHost::system().settle(Duration::from_millis(1));
}

/// A host view that has no wait of its own: everything is the fake host's but the wait.
struct NoWait(FakeHost);

impl HostView for NoWait {
    fn processes(&self) -> Result<Vec<Process>, String> {
        self.0.processes()
    }

    fn mounts(&self) -> Result<Vec<Mount>, String> {
        self.0.mounts()
    }

    fn users(&self) -> Result<BTreeSet<String>, String> {
        self.0.users()
    }

    fn groups(&self) -> Result<BTreeSet<String>, String> {
        self.0.groups()
    }

    fn modules(&self) -> Result<BTreeSet<String>, String> {
        self.0.modules()
    }

    fn watched(&self, dir: &Path) -> Result<Watched, String> {
        self.0.watched(dir)
    }

    fn link(&self, path: &str) -> Result<String, String> {
        self.0.link(path)
    }

    fn file(&self, path: &str) -> Result<String, String> {
        self.0.file(path)
    }
}

/// Asserts that a view that does not wait still gets the second look at once: a process that
/// is there at both looks is a finding, and the wait it was asked for is not recorded
/// anywhere. It catches a second look that depends on the view's wait doing something.
#[test]
fn a_view_with_no_wait_of_its_own_still_looks_twice() {
    let host = FakeHost::quiet();
    let mut checks = Integrity::start(Box::new(NoWait(host.clone())), JOB_USERS, vec![])
        .unwrap()
        .with_settle(WAIT);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/left",
    ));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
    assert!(host.settled().is_empty());
}

/// Asserts that a process still in the control group of the job's own container, which the
/// runtime removed a moment ago (the group is torn down after the pid namespace's processes
/// are gone, so the process table still shows the container's last processes), is not a
/// leftover when it is gone after the wait, and that one that stays is a `daemonised-child`
/// whose finding names its command, state and control group, so that the next incident says
/// what the process was. It catches a finished job's own container read as an escape at the
/// moment it ends, and a finding that names only a pid.
#[test]
fn a_process_of_the_jobs_own_removed_container_is_judged_after_the_wait_and_named() {
    let own = "0::/machine.slice/libpod-own.scope";
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process_named(process(310, 1, 0, 910, own), "js-init", 'Z'));
    host.at_settle(|host| host.end_process(310));
    assert!(after(&mut checks, "r1").is_ok());
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process_named(process(311, 1, 0, 911, own), "sleep", 'S'));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::DAEMONISED_CHILD,
            "daemonised-child: pid 311 (sleep, state S, cgroup 0::/machine.slice/libpod-own.scope) \
             was left behind by job r2 (parent init)"
                .to_owned()
        )
    );
}

/// Asserts that a mount below the directory of a job the agent still holds, there at both
/// looks, is no finding when a transient mount makes the checks wait, and is kept out of the
/// baseline the check takes, so it is a `new-mount` at the next check once that job is no
/// longer held. It catches the second look letting a held job's mount into the baseline.
#[test]
fn a_held_jobs_mount_through_the_wait_stays_out_of_the_baseline() {
    let held = std::path::PathBuf::from("/run/js-agent/mounts/js-a-r2");
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(mount("/run/js-agent/mounts/js-a-r2/rootfs", "overlay"));
    host.mount(fuse_mount("f-6"));
    host.at_settle(|host| host.unmount("/srv/build/tmp/f-6/mnt"));
    let holding = js_agent::lifecycle::Finished {
        run_id: "r1",
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: std::slice::from_ref(&held),
    };
    assert!(js_agent::lifecycle::CheckHook::run(&mut checks, &holding).is_ok());
    assert_eq!(host.settled(), [WAIT]);

    let found = after(&mut checks, "r3").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::NEW_MOUNT,
            "new-mount: /run/js-agent/mounts/js-a-r2/rootfs (ext4) from overlay".to_owned()
        )
    );
}
