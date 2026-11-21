//! The settle wait of the process check: a process of the job users' range that the baseline
//! did not have is a job's leftover only when it is still there after the wait, and the host
//! the checks compare is the one after the wait. Only that check waits: a change that is the
//! host's own is noted from the first look. The host is the in-memory fake, and the wait is a
//! call it records, not time that passes.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use js_agent::integrity::{
    FakeHost, HostView, Integrity, Mount, ProcHost, Process, Watched, code, input,
};
use js_agent::lifecycle::{CheckFinding, CheckHook as _, CheckNote};

use super::{
    JOB_UID, JOB_USERS, after, codes, mount, noted_after, process, process_named, quiet, said,
    started,
};

/// The wait of these tests.
const WAIT: Duration = Duration::from_millis(300);

/// The control group of the agent's own service, below which a job's leftover can be.
const LEFT: &str = "0::/system.slice/agent.service/left";

/// Asserts that a process of the job users' range that is there at the first look and gone
/// after the wait is not a finding (the checks asked for the wait once), and that the same
/// process, still there after the wait, is the `process-as-job-user` it always was. It
/// catches a process that is ending as a job ends tainting a node, and a wait that lets a
/// real leftover go.
#[test]
fn a_process_that_is_gone_after_the_wait_is_not_a_leftover_and_one_that_stays_is() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    host.at_settle(|host| host.end_process(300));
    assert!(quiet(&mut checks, "r1"));
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 300 (stray, state S, cgroup 0::/system.slice/agent.service/left, owner 65534) is running after job r1".to_owned()
        )
    );
    assert_eq!(host.settled(), [WAIT]);
}

/// Asserts that a process whose parent is `init`, of a user outside the job users' range, is
/// noted at the first look with no wait, though the checks were given one. It catches a wait
/// on a note, which delays every job's end for what only the log reads.
#[test]
fn a_daemonised_child_is_noted_without_a_wait() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 1, 0, 900, LEFT));
    host.at_settle(|host| host.end_process(300));
    let notes = noted_after(&mut checks, "r1");
    assert_eq!(codes(&notes), [code::DAEMONISED_CHILD]);
    assert!(host.settled().is_empty(), "nothing asked for a wait");
}

/// Asserts that a process that starts during the wait is not judged by this check and is not
/// part of the baseline it takes, so the next check judges it, with a wait of its own: it is
/// found there when it is still there. It catches a process that arrives during the wait
/// hidden in the baseline for good.
#[test]
fn a_process_that_starts_during_the_wait_is_judged_by_the_next_check() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    host.at_settle(|host| {
        host.end_process(300);
        host.start_process(process(303, 100, JOB_UID, 903, LEFT));
    });
    assert!(quiet(&mut checks, "r1"));

    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 303 (stray, state S, cgroup 0::/system.slice/agent.service/left, owner 65534) is running after job r2".to_owned()
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
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    host.at_settle(|host| host.fail(input::PROCESSES));
    let found = after(&mut checks, "r1").unwrap_err();
    assert!(matches!(found, CheckFinding::Unchecked { .. }), "{found:?}");
    assert_eq!(found.code(), code::PROCESSES_UNREADABLE);
}

/// Asserts that the checks wait only for a leftover process and only when given a wait: a
/// clean host, a host whose only changes are an account added and a new mount, do not wait
/// (they are noted), a job's process that stays waits once and is still a
/// `process-as-job-user`, and checks that were given no wait look once even for a stray
/// process. It catches a wait on every job, a wait on a note that the wait cannot change,
/// and a wait where none was asked for.
#[test]
fn the_checks_wait_only_for_a_leftover_and_only_when_given_a_wait() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    assert!(quiet(&mut checks, "r1"));
    host.add_user("mallory:x:1001:1001::/home/mallory:/bin/sh");
    host.mount(mount("/mnt/x", "/dev/sdb"));
    assert_eq!(
        codes(&noted_after(&mut checks, "r2")),
        [code::NEW_MOUNT, code::USER_CHANGED]
    );
    assert!(host.settled().is_empty());

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(
        after(&mut checks, "r1").unwrap_err().code(),
        code::PROCESS_AS_JOB_USER
    );
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert!(after(&mut checks, "r1").is_err());
    host.mount(mount("/mnt/x", "/dev/sdb"));
    // It would be gone at a wait, which a check given no wait never takes: it is noted.
    host.at_settle(|host| host.unmount("/mnt/x"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/mnt/x (ext4) from /dev/sdb"
        )]
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

/// Asserts that a new mount is noted at the first look, whole, with no wait, whether it is a
/// FUSE mount a host process holds for a moment or the mount of the finished job's own file
/// system tree (below its directory of the runtime's mount root, which no held job names),
/// and that the check passes; and that once the mount is gone the next check notes nothing
/// (a mount that went away is no note). It catches a taint for a mount, and a wait on a mount.
#[test]
fn a_new_mount_is_noted_at_the_first_look_without_a_wait() {
    let tree = Mount {
        root: "/".to_owned(),
        point: "/run/js-agent/mounts/js-a-r1/rootfs".to_owned(),
        fstype: "overlay".to_owned(),
        source: "overlay".to_owned(),
    };
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(fuse_mount("a-1"));
    host.mount(fuse_mount("b-2"));
    host.mount(tree);
    assert_eq!(
        noted_after(&mut checks, "r1"),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/run/js-agent/mounts/js-a-r1/rootfs (overlay) from overlay, /srv/build/tmp/a-1/mnt (fuse.testfs) from testfs, /srv/build/tmp/b-2/mnt (fuse.testfs) from testfs"
        )]
    );
    assert!(host.settled().is_empty());
    host.unmount("/srv/build/tmp/a-1/mnt");
    host.unmount("/srv/build/tmp/b-2/mnt");
    host.unmount("/run/js-agent/mounts/js-a-r1/rootfs");
    assert!(quiet(&mut checks, "r2"));
}

/// Asserts that what changes during the wait a leftover process asked for is part of the
/// second look the checks compare: a mount made during the wait is noted by the check that
/// waited, not by the next one, and a mount of the baseline that is gone at the first look and
/// back after the wait is no note. It catches a second look that compares the first look's
/// mounts, and a baseline mount read as new when a host service remounts it as a job ends.
#[test]
fn what_changes_during_the_wait_is_part_of_the_second_look() {
    let host = FakeHost::quiet();
    host.mount(mount("/run/tierstore/fs", "tierstore"));
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    host.unmount("/run/tierstore/fs");
    host.at_settle(|host| {
        host.mount(mount("/run/tierstore/fs", "tierstore"));
        host.mount(mount("/mnt/late", "/dev/sdc"));
    });
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/mnt/late (ext4) from /dev/sdc"
        )]
    );
    assert_eq!(host.settled(), [WAIT]);
}

/// Asserts that a mount table that cannot be read at the second look is a note, not a failure
/// of the check: the leftover process is still the finding, and the note names the mounts as
/// unreadable. It catches a failed second look read as a host with no new mount, and a taint
/// for an input that no longer taints.
#[test]
fn a_second_look_that_cannot_read_the_mounts_is_a_note() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    host.at_settle(|host| host.fail(input::MOUNTS));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::MOUNTS_UNREADABLE,
            "mounts: cannot be read"
        )]
    );
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
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert!(host.settled().is_empty());
}

/// Asserts that a process still in the control group of the job's own container, which the
/// runtime removed a moment ago (the group is torn down after the pid namespace's processes
/// are gone, so the process table still shows the container's last processes), is not a
/// leftover when it is gone after the wait, and that one that stays is a
/// `process-as-job-user` whose finding names its command, state, control group, owner and
/// command line, so that the next incident says what the process was. It catches a finished
/// job's own container read as an escape at the moment it ends, and a finding that names only
/// a pid.
#[test]
fn a_process_of_the_jobs_own_removed_container_is_judged_after_the_wait_and_named() {
    let own = "0::/machine.slice/libpod-own.scope";
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process_named(
        process(310, 100, JOB_UID, 910, own),
        "js-init",
        'Z',
    ));
    host.at_settle(|host| host.end_process(310));
    assert!(quiet(&mut checks, "r1"));
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process_named(
        process(311, 100, JOB_UID, 911, own),
        "sleep",
        'S',
    ));
    host.set_cmdline(311, "sleep 3600");
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 311 (sleep, state S, cgroup 0::/machine.slice/libpod-own.scope, owner 65534, cmd \"sleep 3600\") \
             is running after job r2"
                .to_owned()
        )
    );
}

/// Asserts that a mount below the directory of a job the agent still holds is no note while
/// that job is held, and is kept out of the baseline the check takes, so it is a `new-mount`
/// note at the next check once that job is no longer held. It catches a held job's mount
/// taken into the baseline.
#[test]
fn a_held_jobs_mount_stays_out_of_the_baseline() {
    let held = std::path::PathBuf::from("/run/js-agent/mounts/js-a-r2");
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.mount(mount("/run/js-agent/mounts/js-a-r2/rootfs", "overlay"));
    let holding = js_agent::lifecycle::Finished {
        run_id: "r1",
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: std::slice::from_ref(&held),
    };
    assert!(js_agent::lifecycle::CheckHook::run(&mut checks, &holding).is_ok());
    assert!(js_agent::lifecycle::CheckHook::notes(&mut checks).is_empty());

    assert_eq!(
        noted_after(&mut checks, "r3"),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/run/js-agent/mounts/js-a-r2/rootfs (ext4) from overlay"
        )]
    );
}
