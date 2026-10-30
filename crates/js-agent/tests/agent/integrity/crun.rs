//! The direct runner's layout and what it does to the post-job checks: a lease's container
//! is the control group `<actions>/<lease>/ctr`, the agent sits in a group of its own beside
//! `actions/` (not above it), and the runner's own processes (`crun run`, `crun kill`, the
//! keeper) are children of the agent in the agent's group. The fake host's process table and
//! `/proc/self/cgroup` stand in for the machine; nothing waits.

use js_agent::integrity::{FakeHost, Integrity, code};

use super::{JOB_UID, after, beside, process, started};

/// The agent's group: beside `actions/`, as the delegated tree makes it.
const AGENT: &str = "0::/system.slice/js.service/supervisor\n";

/// The pid the fake host gives the agent.
const AGENT_PID: u32 = 100;

/// A host whose agent is in [`AGENT`] and whose checks have taken their baseline.
fn host_and_checks() -> (FakeHost, Integrity) {
    let host = FakeHost::quiet();
    host.set_file("/proc/self/cgroup", AGENT);
    let checks = started(&host, vec![]);
    (host, checks)
}

/// Asserts that a process of a job, left after its lease ended, counts wherever the lease's
/// group sits: `<lease>/ctr` and a group below it, the lease's own group, the `actions/`
/// group itself, and a `<lease>/ctr` with no `actions/` in the path. The agent's group is
/// beside `actions/` here, under a system service the checks otherwise treat as the host's:
/// a process there passed unseen. It catches a rule that knows a container only by an
/// engine's name, which lets a leftover job process pass after every run.
#[test]
fn a_job_process_left_under_actions_counts() {
    for cgroup in [
        "0::/system.slice/js.service/actions/lease-r1/ctr",
        "0::/system.slice/js.service/actions/lease-1-2/ctr",
        "0::/system.slice/js.service/actions/lease-r1/ctr/sub",
        "0::/system.slice/js.service/actions/lease-r1",
        "0::/system.slice/js.service/actions",
        "0::/system.slice/js.service/elsewhere/lease-r1/ctr",
        "0::/user.slice/user-1000.slice/lease-r1/ctr",
        "1:name=systemd:/x 0::/system.slice/js.service/actions/lease-r1/ctr",
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(300, 1, 0, 900, cgroup));
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{cgroup}");
    }
}

/// Asserts that names which only look like the direct runner's do not make a host service's
/// process a job's: a group that starts like `actions`, a `ctr` that is not a lease's
/// child, a lease group that has a longer name than `ctr` below it, and a parent that only
/// ends in `lease-`. It catches a substring match, which would taint a node for what its own
/// services do.
#[test]
fn lookalike_names_are_the_hosts() {
    for cgroup in [
        "0::/system.slice/js.service/actions-old/lease-r1",
        "0::/system.slice/js.service/ctr",
        "0::/system.slice/js.service/lease-r1/ctrl",
        "0::/system.slice/js.service/xlease-r1/ctr",
        "0::/system.slice/js.service/lease/ctr2",
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(300, 1, 0, 900, cgroup));
        assert!(after(&mut checks, "r1").is_ok(), "{cgroup}");
    }
}

/// Asserts that a neighbour's container, still running when a job ends, is not the job's
/// stray, and the job's own is: with `r2` running, its init in `<lease-r2>/ctr` passes, and
/// a process left in `<lease-r1>/ctr` is found. It catches a rule that excuses every
/// `<lease>/ctr` (a concurrent job would hide a leftover) and one that excuses none (a
/// concurrent job would taint the node).
#[test]
fn a_neighbours_ctr_is_not_a_stray_and_the_jobs_own_is() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/js.service/actions/lease-r2/ctr",
    ));
    assert!(beside(&mut checks, "r1", &["r2"]).is_ok());
    host.start_process(process(
        301,
        1,
        0,
        901,
        "0::/system.slice/js.service/actions/lease-r1/ctr",
    ));
    let found = beside(&mut checks, "r1", &["r2"]).unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
}

/// Asserts which processes are the runner's own noise: a child of the agent (its parent is
/// alive and is not `init`) that is in the agent's own group, as a neighbour's `crun run`,
/// a `crun kill` or `crun delete` and the keeper are while a job ends, and that runs as the
/// job's user (the runner drops to it). Not noise: the same process with `init` as its
/// parent (the agent no longer holds it), a process of the job's user in a group below the
/// agent's, and one in a container's group. It catches a check that counts the runner's own
/// processes (every concurrent job would taint the node) and an exemption that is wider
/// than the agent's own group.
#[test]
fn the_runners_own_processes_are_noise_and_nothing_wider() {
    let own = "0::/system.slice/js.service/supervisor";
    let (host, mut checks) = host_and_checks();
    host.start_process(process(310, AGENT_PID, JOB_UID, 910, own));
    host.start_process(process(311, 310, JOB_UID, 911, own));
    assert!(after(&mut checks, "r1").is_ok(), "crun and its child");

    for (what, ppid, cgroup) in [
        ("orphaned", 1, own),
        (
            "below the agent's group",
            AGENT_PID,
            "0::/system.slice/js.service/supervisor/x",
        ),
        (
            "in a container's group",
            AGENT_PID,
            "0::/system.slice/js.service/actions/lease-r1/ctr",
        ),
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(320, ppid, JOB_UID, 920, cgroup));
        assert!(after(&mut checks, "r1").is_err(), "{what}");
    }
}
