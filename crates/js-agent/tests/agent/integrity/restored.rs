//! The re-check of a taint read from the marker at start ([`CheckHook::run_restored`]): the
//! baseline of the restarted agent holds what the tainting job left, so it exempts no process
//! from the two rules that need no baseline, a uid of the job users' range and a parent
//! `init` in a group a job can be in.

use std::time::Duration;

use js_agent::integrity::{FakeHost, Integrity, code};
use js_agent::lifecycle::{CheckFinding, CheckHook as _, Finished};

use super::{JOB_UID, after, process, started};

/// The wait of these tests.
const WAIT: Duration = Duration::from_millis(300);

/// The control group the fake host gives the agent.
const AGENT_GROUP: &str = "0::/system.slice/agent.service";

/// A control group of a container, where a job's process can be.
const CONTAINER_GROUP: &str = "0::/machine.slice/libpod-9e1.scope";

/// The re-check of a restored taint, with nothing live.
fn restored(checks: &mut Integrity, run_id: &str) -> Result<(), CheckFinding> {
    checks.run_restored(&Finished {
        run_id,
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: &[],
    })
}

/// Asserts that a process of the job users' range that the baseline holds passes the normal
/// check and is a `process-as-job-user` at the restored one, whatever its group or parent.
/// Catches a restored re-check that trusts the restart baseline, which holds the process the
/// tainting job left.
#[test]
fn a_job_user_process_in_the_baseline_is_found_at_a_restored_recheck() {
    let host = FakeHost::quiet();
    host.start_process(process(300, 100, JOB_UID, 900, CONTAINER_GROUP));
    let mut checks = started(&host, vec![]);
    assert!(after(&mut checks, "r1").is_ok(), "the baseline holds it");
    let found = restored(&mut checks, "taint-recheck").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert!(found.detail().contains("pid 300"), "{found:?}");
}

/// Asserts that a process with parent `init` in the agent's own group or below it, or in a
/// container's group, that the baseline holds is a `daemonised-child` at the restored
/// re-check and not at the normal one. Catches a daemonised child of the tainting job that
/// hides in the restart baseline.
#[test]
fn a_parent_init_process_in_a_job_group_in_the_baseline_is_found_at_a_restored_recheck() {
    for cgroup in [
        "0::/system.slice/agent.service",
        "0::/system.slice/agent.service/leftover",
        "0::/system.slice/js.service/actions/js-0123456789abcdef-r1/ctr",
        CONTAINER_GROUP,
        "1:name=systemd:/x",
    ] {
        let host = FakeHost::quiet();
        host.start_process(process(300, 1, 0, 900, cgroup));
        let mut checks = started(&host, vec![]);
        assert!(after(&mut checks, "r1").is_ok(), "{cgroup}: baseline");
        let found = restored(&mut checks, "taint-recheck").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{cgroup}");
    }
}

/// Asserts what a restored re-check leaves alone: a process with parent `init` in a group no
/// job can be in (the root group, `init.scope`, a scope of the system slice, a Podman
/// monitor, a login's session), a process of another user whose parent is not `init`, and
/// the runner's own child (a child of the agent in its group, even as the job user), all in
/// the baseline. Catches a baseline-free rule that is wider than the two it is made of.
#[test]
fn a_restored_recheck_leaves_the_hosts_own_processes_alone() {
    let host = FakeHost::quiet();
    for (pid, ppid, uid, cgroup) in [
        (300, 1, 0, "0::/"),
        (301, 1, 0, "0::/init.scope"),
        (302, 1, 0, "0::/system.slice/foo.scope"),
        (303, 1, 0, "0::/machine.slice/libpod-conmon-4f3a9c.scope"),
        (304, 1, 0, "0::/user.slice/user-1000.slice/session-7.scope"),
        (305, 100, 2000, AGENT_GROUP),
        (306, 100, JOB_UID, AGENT_GROUP),
    ] {
        host.start_process(process(pid, ppid, uid, 900 + u64::from(pid), cgroup));
    }
    let mut checks = started(&host, vec![]);
    assert!(restored(&mut checks, "taint-recheck").is_ok());
}

/// Asserts that the agent's own process (parent `init` in its own group, as a service's main
/// process is) is not a finding at a restored re-check, and that it is one when the checks
/// could not read which process is the agent's (`/proc/self/stat` missing or not text with a
/// pid first). Catches a restored re-check that taints every host for the agent itself, and
/// one that quietly exempts a pid it never learned.
#[test]
fn the_agents_own_process_is_not_a_finding() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    assert!(restored(&mut checks, "taint-recheck").is_ok());

    for stat in [None, Some("not a pid (js-agent) S 1")] {
        let host = FakeHost::quiet();
        match stat {
            Some(text) => host.set_file("/proc/self/stat", text),
            None => host.remove_file("/proc/self/stat"),
        }
        let mut checks = started(&host, vec![]);
        let found = restored(&mut checks, "taint-recheck").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{stat:?}");
        assert!(found.detail().contains("pid 100"), "{found:?}");
    }
}

/// Asserts that a restored re-check waits and looks again like the normal one: a baseline
/// process of the job user that is gone at the second look is not a finding, and one that is
/// still there is. Catches a restored re-check that judges the first look alone, and so
/// taints a host for a process that ends a moment later.
#[test]
fn a_restored_recheck_judges_what_is_there_after_the_wait() {
    let host = FakeHost::quiet();
    host.start_process(process(300, 100, JOB_UID, 900, CONTAINER_GROUP));
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.at_settle(|host| host.end_process(300));
    assert!(restored(&mut checks, "taint-recheck").is_ok());
    assert_eq!(host.settled(), [WAIT]);

    let host = FakeHost::quiet();
    host.start_process(process(300, 100, JOB_UID, 900, CONTAINER_GROUP));
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    let found = restored(&mut checks, "taint-recheck").unwrap_err();
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert_eq!(host.settled(), [WAIT]);
}
