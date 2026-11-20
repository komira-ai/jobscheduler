//! The direct runner's layout and what it does to the post-job checks: a lease's container
//! is the control group `<actions>/<lease>/ctr`, the agent sits in a group of its own beside
//! `actions/` (not above it), and the runner's own processes (`crun run`, `crun kill`, the
//! keeper) are children of the agent in the agent's group. The fake host's process table and
//! `/proc/self/cgroup` stand in for the machine; nothing waits.

use js_agent::integrity::{FakeHost, Integrity, code};
use js_agent::lifecycle::CheckHook as _;

use super::{
    JOB_UID, after, codes, noted_after, noted_beside, process, quiet, quiet_beside, started,
};

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

/// Asserts that a process of a job, left after its lease ended, is noted wherever the lease's
/// group sits: `<lease>/ctr` and a group below it, the lease's own group, the `actions/`
/// group itself, and a `<lease>/ctr` with no `actions/` in the path. The agent's group is
/// beside `actions/` here, under a system service the checks otherwise treat as the host's:
/// a process there passed unseen. It catches a rule that knows a container only by an
/// engine's name, which leaves a job's process out of the log after every run.
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
        assert_eq!(
            codes(&noted_after(&mut checks, "r1")),
            [code::DAEMONISED_CHILD],
            "{cgroup}"
        );
    }
}

/// Asserts that names which only look like the direct runner's do not make a host service's
/// process a job's: a group that starts like `actions`, a `ctr` that is not a lease's
/// child, a lease group that has a longer name than `ctr` below it, and a parent that only
/// ends in `lease-`. It catches a substring match, which would log a line for what the host's
/// own services do.
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
        assert!(quiet(&mut checks, "r1"), "{cgroup}");
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
/// stray, and the job's own is: with `r2` running, its init in `<its leaf>/ctr` is no note,
/// and a process left in `<the job's leaf>/ctr` is a `daemonised-child` note. It catches a rule that excuses every
/// `<lease>/ctr` (a concurrent job would hide a leftover) and one that excuses none (a
/// concurrent job would fill the log).
#[test]
fn a_neighbours_ctr_is_not_a_stray_and_the_jobs_own_is() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(300, 1, 0, 900, &ctr_of("r2")));
    assert!(quiet_beside(&mut checks, "r1", &[&leaf("r2")]));
    host.start_process(process(301, 1, 0, 901, &ctr_of("r1")));
    assert_eq!(
        codes(&noted_beside(&mut checks, "r1", &[&leaf("r2")])),
        [code::DAEMONISED_CHILD]
    );
}

/// Asserts that a live container is named by a whole path component, not by a substring: with
/// `run-3-1` running, a process left in the group of the finished `run-3-12` is noted, and
/// one in the group of `run-3-1` is not. Server run ids are `run-<term>-<count>`, so one is
/// often a prefix of another. It catches a neighbour's handle excusing the leftover of any
/// run whose id it is a prefix of.
#[test]
fn a_live_handle_excuses_its_own_group_and_not_a_longer_name() {
    let (host, mut checks) = host_and_checks();
    host.start_process(process(300, 1, 0, 900, &ctr_of("run-3-1")));
    assert!(quiet_beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]));
    host.start_process(process(301, 1, 0, 901, &ctr_of("run-3-12")));
    let notes = noted_beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]);
    assert_eq!(codes(&notes), [code::DAEMONISED_CHILD]);
    assert!(notes[0].detail.starts_with("pid 301"), "{notes:?}");
    assert!(!notes[0].detail.contains("pid 300"), "{notes:?}");
}

/// Asserts the same for mounts: the mounts of a running container's lease directory (the
/// handle is a component of the mount's point) are not the finished job's, and a mount in
/// the directory of a longer name is a `new-mount` note. Engines' scopes and ids are components
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
    assert!(quiet_beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]));
    host.mount(mount(under("run-3-12")));
    let notes = noted_beside(&mut checks, "run-3-12", &[&leaf("run-3-1")]);
    assert_eq!(codes(&notes), [code::NEW_MOUNT]);
    assert!(notes[0].detail.contains(&leaf("run-3-12")), "{notes:?}");

    for (cgroup, excused) in [
        ("0::/machine.slice/libpod-9e1.scope", true),
        ("0::/system.slice/docker-9e1.scope", true),
        ("0::/docker/9e1", true),
        ("0::/libpod_parent/libpod-9e1", true),
        ("0::/machine.slice/libpod-9e12.scope", false),
        ("0::/docker/9e12", false),
        ("0::/machine.slice/libpod-conmon-9e1.scope/x", false),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.start_process(process(400, 1, 0, 950, cgroup));
        assert_eq!(
            quiet_beside(&mut checks, "r1", &["9e1"]),
            excused,
            "{cgroup}"
        );
    }
}

/// Asserts that a helper container's processes (unpack and removal helpers, in
/// `actions/helpers/<name>`) are the runner's, not a stray, whatever their user (the job
/// user's included): a background pull can have one running when another job's check runs.
/// Not the runner's: a group that only starts like `helpers`, an `actions/helpers` pair
/// that is not the real actions group (one below a lease's `ctr`, or below a lease's
/// group), and a lease that has `helpers` in it; there a process of the job user is a
/// finding and one of root, whose parent is `init`, is a `daemonised-child` note.
#[test]
fn a_helper_container_is_the_runners() {
    for cgroup in [
        "0::/system.slice/js.service/actions/helpers/helper-unpack-5-1/ctr",
        // Services whose names start like a lease's group but are not one.
        "0::/js-agent.slice/js-short-x.service/js-zzzzzzzzzzzzzzzz-r1/js-0123456789abcdef-/actions/helpers/x",
    ] {
        for uid in [0, JOB_UID] {
            let (host, mut checks) = host_and_checks();
            host.start_process(process(300, 1, uid, 900, cgroup));
            assert!(quiet(&mut checks, "r1"), "{cgroup} uid {uid}");
        }
    }
    for cgroup in [
        "0::/system.slice/js.service/actions/helpers-x/helper-unpack-5-1",
        "0::/system.slice/js.service/actions/js-0123456789abcdef-r1/helpers/x",
        "0::/system.slice/js.service/actions/js-0123456789abcdef-r1/ctr/actions/helpers/x",
        "0::/system.slice/js.service/ctr/actions/helpers/x",
        "0::/system.slice/js.service/actions/js-0123456789abcdef-r1/actions/helpers/x",
    ] {
        let (host, mut checks) = host_and_checks();
        host.start_process(process(301, 1, 0, 901, cgroup));
        assert_eq!(
            codes(&noted_after(&mut checks, "r1")),
            [code::DAEMONISED_CHILD],
            "{cgroup} uid 0"
        );
        let (host, mut checks) = host_and_checks();
        host.start_process(process(301, 1, JOB_UID, 901, cgroup));
        assert!(after(&mut checks, "r1").is_err(), "{cgroup} uid {JOB_UID}");
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
    assert!(quiet(&mut checks, "r1"), "crun and its child");

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

/// The check after `run_id`, with the runtime mounting the file systems of other held jobs
/// below `held`.
fn holding(
    checks: &mut Integrity,
    run_id: &str,
    held: &[&str],
) -> Result<(), js_agent::lifecycle::CheckFinding> {
    let held: Vec<std::path::PathBuf> = held.iter().map(std::path::PathBuf::from).collect();
    checks.run(&js_agent::lifecycle::Finished {
        run_id,
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: &held,
    })
}

/// Asserts that a mount whose point lies below the directory where the runtime mounts the
/// file system of another job the agent holds (`<mounts>/<lease>`, a job that has no
/// container yet among them) is not the finished job's, and is kept out of the baseline, so
/// that one left there once that job is no longer held is a `new-mount` note; and that a mount at
/// that directory itself, below a directory whose name only begins like it, or anywhere
/// else, is a `new-mount` note. Catches a waiting job's clones and overlays taken for its
/// neighbour's leftovers, and a held lease that excuses a mount of any name it prefixes.
#[test]
fn a_mount_below_a_held_jobs_mounts_is_its_and_no_other_jobs() {
    let mount = |point: &str| js_agent::integrity::Mount {
        root: "/".to_owned(),
        point: point.to_owned(),
        fstype: "overlay".to_owned(),
        source: "overlay".to_owned(),
    };
    let held = format!("/run/js-agent/mounts/{}", leaf("run-4-1"));
    let (host, mut checks) = host_and_checks();
    host.mount(mount(&format!("{held}/src/0")));
    host.mount(mount(&format!("{held}/rootfs")));
    assert_eq!(holding(&mut checks, "run-4-2", &[&held]), Ok(()));
    assert!(checks.notes().is_empty(), "the held job's own");
    // No longer held, its leftover is noted by the next check: it never joined the baseline.
    assert_eq!(holding(&mut checks, "run-4-3", &[]), Ok(()));
    let notes = checks.notes();
    assert_eq!(codes(&notes), [code::NEW_MOUNT]);
    assert!(
        notes[0].detail.contains(&format!("{held}/rootfs")),
        "{notes:?}"
    );
    for point in [
        held.clone(),
        format!("{held}2/src/0"),
        "/mnt/elsewhere".to_owned(),
    ] {
        let (host, mut checks) = host_and_checks();
        host.mount(mount(&point));
        assert_eq!(holding(&mut checks, "run-4-2", &[&held]), Ok(()));
        assert_eq!(codes(&checks.notes()), [code::NEW_MOUNT], "{point}");
    }
}
