//! The settle wait of the process checks: a process the baseline did not have is a job's
//! leftover only when it is still there after the wait, and the host the checks compare is
//! the one after the wait. The host is the in-memory fake, and the wait is a call it records,
//! not time that passes.

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

/// Asserts that the checks wait only when a process is what they would find: a clean host
/// and a host with a new mount do not wait, and checks that were given no wait look once even
/// for a stray process. It catches a wait on every job and a wait where none was asked for.
#[test]
fn the_checks_wait_only_for_a_process_and_only_when_given_a_wait() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    assert!(after(&mut checks, "r1").is_ok());
    host.mount(mount("/mnt/x", "/dev/sdb"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::NEW_MOUNT);
    assert!(host.settled().is_empty());

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
    assert!(host.settled().is_empty());
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
