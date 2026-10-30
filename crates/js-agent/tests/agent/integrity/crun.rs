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
        "0::/system.slice/js.service/elsewhere/lease-r1/ctr",
        "0::/system.slice/js.service/elsewhere/js-0123456789abcdef-r1/ctr",
        "0::/system.slice/js.service/actions-x/js-0123456789abcdef-r1/ctr",
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(300, 1, 0, 900, cgroup));
        assert!(after(&mut checks, "r1").is_ok(), "{cgroup}");
    }
}

/// The container name the direct runner gives run `run`: `js-<agent tag>-<run id>`, which is
/// also the name of its lease's control group and the run handle.
fn leaf(run: &str) -> String {
    format!("js-0123456789abcdef-{run}")
}

/// A process's control groups for the container of run `run`.
fn ctr_of(run: &str) -> String {
    format!("0::/system.slice/js.service/actions/{}/ctr", leaf(run))
}

/// Asserts that a neighbour's container, still running when a job ends, is not the job's
/// stray, and the job's own is: with `r2` running, its init in `<its leaf>/ctr` passes, and a
/// process left in `<the job's leaf>/ctr` is found. It catches a rule that excuses every
/// `<lease>/ctr` (a concurrent job would hide a leftover) and one that excuses none (a
/// concurrent job would taint the node).
#[test]
fn a_neighbours_ctr_is_not_a_stray_and_the_jobs_own_is() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(300, 1, 0, 900, &ctr_of("r2")));
    assert!(beside(&mut checks, "r1", &[&leaf("r2")]).is_ok());
    host.start_process(process(301, 1, 0, 901, &ctr_of("r1")));
    let found = beside(&mut checks, "r1", &[&leaf("r2")]).unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
}

/// Asserts that a live container is named by a whole path component, not by a substring: with
/// `run-3-1` running, a process left in the group of the finished `run-3-12` is found, and
/// one in the group of `run-3-1` is not. Server run ids are `run-<term>-<count>`, so one is
/// often a prefix of another. It catches a neighbour's handle excusing the leftover of any
/// run whose id it is a prefix of.
#[test]
fn a_live_handle_excuses_its_own_group_and_not_a_longer_name() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(300, 1, 0, 900, &ctr_of("run-3-1")));
    assert!(beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]).is_ok());
    host.start_process(process(301, 1, 0, 901, &ctr_of("run-3-12")));
    let found = beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]).unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
    assert!(found.detail().contains("pid 301"), "{found:?}");
    assert!(!found.detail().contains("pid 300"), "{found:?}");
}

/// Asserts the same for mounts: the mounts of a running container's lease directory (the
/// handle is a component of the mount's point) are not the finished job's, and a mount in
/// the directory of a longer name is a `new-mount`. Engines' scopes and ids are components
/// too: `libpod-<id>.scope` names the container `<id>`, and `libpod-<id>2.scope` does not.
#[test]
fn a_live_handle_excuses_its_own_mounts_and_not_a_longer_names() {
    let mount = |point: String| js_agent::integrity::Mount {
        root: "/".to_owned(),
        point,
        fstype: "overlay".to_owned(),
        source: "overlay".to_owned(),
    };
    let under = |run: &str| format!("/var/lib/js/leases/{}/rootfs", leaf(run));
    let (host, mut checks) = host_and_checks();
    host.mount(mount(under("run-3-1")));
    assert!(beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]).is_ok());
    host.mount(mount(under("run-3-12")));
    let found = beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]).unwrap_err();
    assert_eq!(found.code(), code::NEW_MOUNT);
    assert!(found.detail().contains(&leaf("run-3-12")), "{found:?}");

    for (cgroup, excused) in [
        ("0::/machine.slice/libpod-9e1.scope", true),
        ("0::/system.slice/docker-9e1.scope", true),
        ("0::/docker/9e1", true),
        ("0::/machine.slice/libpod-9e12.scope", false),
        ("0::/docker/9e12", false),
        ("0::/machine.slice/libpod-conmon-9e1.scope/x", false),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.start_process(process(400, 1, 0, 950, cgroup));
        assert_eq!(
            beside(&mut checks, "r1", &["9e1"]).is_ok(),
            excused,
            "{cgroup}"
        );
    }
}

/// Asserts that a helper container's processes (unpack and removal helpers, in
/// `actions/helpers/<name>`) are the runner's, not a stray: a background pull can have one
/// running when another job's check runs. A group that only starts like `helpers`, and the
/// helpers group of another `actions`, are not.
#[test]
fn a_helper_container_is_the_runners() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/js.service/actions/helpers/helper-unpack-5-1/ctr",
    ));
    assert!(after(&mut checks, "r1").is_ok());
    for cgroup in [
        "0::/system.slice/js.service/actions/helpers-x/helper-unpack-5-1",
        "0::/system.slice/js.service/actions/js-0123456789abcdef-r1/helpers/x",
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(301, 1, 0, 901, cgroup));
        assert!(after(&mut checks, "r1").is_err(), "{cgroup}");
    }
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
