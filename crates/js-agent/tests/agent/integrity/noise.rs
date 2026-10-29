//! What a host does on its own while jobs run: the services its timers start, the logins of
//! its operators, Podman's monitors and network namespaces. None of it is a job's, and none
//! of it may taint the host; anything in a container's own control group still does.

use js_agent::integrity::{FakeHost, Integrity, Mount, code};

use super::{after, beside, process, started};

/// A mount of `fstype` at `point`, of `root` from `source`.
fn mounted(fstype: &str, point: &str, root: &str, source: &str) -> Mount {
    Mount {
        root: root.to_owned(),
        point: point.to_owned(),
        fstype: fstype.to_owned(),
        source: source.to_owned(),
    }
}

/// Asserts that processes whose parent is `init`, started after the baseline in the host's
/// own control groups, are not findings: a timer's service in the system slice, an
/// operator's login session and `systemd --user` in the user slice, and a Podman monitor
/// that outlives its container. Each is a `daemonised-child` on a host that judges a
/// process by its parent alone, which taints a node within a minute of its first job.
#[test]
fn the_hosts_own_services_logins_and_monitors_are_not_a_jobs() {
    for cgroup in [
        "0::/system.slice/disk-alarm.service",
        "0::/system.slice/apt-daily-upgrade.service",
        "0::/system.slice/k3s-agent.service/containerd-shim",
        "0::/user.slice/user-1000.slice/session-7.scope",
        "0::/user.slice/user-1000.slice/user@1000.service/init.scope",
        "1:name=systemd:/user.slice 0::/user.slice/user-1000.slice/user@1000.service",
        "0::/machine.slice/libpod-conmon-4f3a9c.scope",
        "0::/system.slice/libpod-conmon-4f3a9c.scope",
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.start_process(process(300, 1, 0, 900, cgroup));
        host.start_process(process(301, 1, super::JOB_UID, 901, cgroup));
        assert!(after(&mut checks, "r1").is_ok(), "{cgroup}");
    }
}

/// Asserts that a process whose parent is `init` still counts when its control group names
/// a container, wherever the container's group is (Podman's and Docker's scopes, cgroupfs
/// groups, a rootless container in the user slice, a container under a system service),
/// when it is in a scope under a system service, in the root group, in a slice itself, or
/// has no unified-hierarchy line. It catches an exemption by prefix alone, which would let
/// a job's leftover process pass because its container sat under a host slice.
#[test]
fn a_process_in_a_containers_group_counts_wherever_it_is() {
    for cgroup in [
        "0::/machine.slice/libpod-4f3a9c.scope",
        "0::/machine.slice/libpod-4f3a9c.scope/container",
        "0::/system.slice/docker-4f3a9c.scope",
        "0::/docker/4f3a9c",
        "0::/libpod_parent/libpod-4f3a9c",
        "0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-4f3a9c.scope",
        "0::/system.slice/agent.service/libpod-4f3a9c",
        "0::/system.slice/agent.service/run-r1.scope",
        "0::/system.slice/run-r1.scope",
        "0::/system.slice",
        "0::/",
        "1:name=systemd:/system.slice/x.service",
        "",
        "0::/system.slice/libpod-conmon-4f3a9c",
        "0::/system.slice/x.service/docker",
        "0::/system.slice/x.service/libpod_parent",
        "0::/user.slice/user-1000.slice/machine.slice",
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.start_process(process(300, 1, 0, 900, cgroup));
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{cgroup}");
    }
}

/// Asserts that the mounts a host makes while jobs run are not findings: a network
/// namespace Podman pins for a container (its own or a neighbour's: the name holds no
/// container id), the bind of `/run/netns` onto itself the first container after a boot
/// makes, and the run directory of a user who logs in. Each is a `new-mount` on a host that
/// counts every new mount, which taints a node at its first job after a boot, at an
/// operator's login, or when two jobs overlap.
#[test]
fn podmans_namespaces_and_a_logins_run_directory_are_not_a_jobs() {
    for mount in [
        mounted(
            "nsfs",
            "/run/netns/netns-0b6e2f6c-1d1a-4b7e-9d43-3d5c8a1f2e70",
            "net:[4026532871]",
            "nsfs",
        ),
        mounted("nsfs", "/run/netns/cni-5f1c", "net:[4026532901]", "nsfs"),
        mounted("tmpfs", "/run/netns", "/netns", "tmpfs"),
        mounted("tmpfs", "/run/user/1000", "/", "tmpfs"),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.mount(mount.clone());
        assert!(after(&mut checks, "r1").is_ok(), "{mount:?}");
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.mount(mount.clone());
        assert!(beside(&mut checks, "r1", &["c2"]).is_ok(), "{mount:?}");
    }
}

/// Asserts that a mount near the exempt ones still counts: a namespace mounted elsewhere or
/// deeper under `/run/netns`, another file system there, a tmpfs mounted over `/run/netns`
/// (not its bind onto itself), and a mount inside or beside a user's run directory. It
/// catches an exemption by mount point alone, which would let a job's mount pass.
#[test]
fn a_mount_near_the_hosts_own_still_counts() {
    for mount in [
        mounted("nsfs", "/tmp/ns", "net:[4026532871]", "nsfs"),
        mounted("nsfs", "/run/netns/a/b", "net:[4026532871]", "nsfs"),
        mounted("nsfs", "/run/netns/", "net:[4026532871]", "nsfs"),
        mounted("ext4", "/run/netns/x", "/", "/dev/sdb1"),
        mounted("tmpfs", "/run/netns", "/", "tmpfs"),
        mounted("tmpfs", "/run/user/1000/doc", "/", "tmpfs"),
        mounted("tmpfs", "/run/user/x", "/", "tmpfs"),
        mounted("tmpfs", "/run/user/", "/", "tmpfs"),
        mounted("ext4", "/run/user/1000", "/", "/dev/sdb1"),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.mount(mount.clone());
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(found.code(), code::NEW_MOUNT, "{mount:?}");
    }
}

/// Asserts that a process left in the agent's own control group, or in a group below it,
/// still counts, though the agent's group is a service of the system slice, while one in
/// another service, and a Podman monitor's scope below the agent's group (containers
/// parented under the agent), do not: a child of the agent that outlives it is judged like
/// any other process. It catches an exemption of every service, the agent's included, and
/// a monitor counted because it sits under the agent.
#[test]
fn the_agents_own_service_is_not_the_hosts() {
    for cgroup in [
        "0::/system.slice/agent.service",
        "0::/system.slice/agent.service/payload",
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        host.start_process(process(300, 1, 0, 900, cgroup));
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{cgroup}");
    }
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service/libpod-conmon-4f3a9c.scope",
    ));
    assert!(
        after(&mut checks, "r0").is_ok(),
        "a Podman monitor under the agent's group"
    );
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(
        300,
        1,
        0,
        900,
        "0::/system.slice/agent.service-two.service",
    ));
    assert!(after(&mut checks, "r1").is_ok(), "another service");
}

/// Asserts that an agent that cannot read its own control group does not start, saying
/// why. It catches checks that would exempt the agent's own service unseen.
#[test]
fn an_agent_that_cannot_read_its_own_control_group_does_not_start() {
    let host = FakeHost::quiet();
    host.remove_file("/proc/self/cgroup");
    let why = Integrity::start(Box::new(host), super::JOB_UID, vec![])
        .err()
        .unwrap();
    assert!(
        why.starts_with("cannot read the agent's own control group: /proc/self/cgroup"),
        "{why}"
    );
}

/// Asserts that an agent whose own control group names no unified path, or the root, starts
/// and exempts no group for being its own: a service of the system slice is the host's as
/// it is for any agent, and a process in the root group still counts. It catches a root
/// path taken as the agent's group, which would make every process the agent's.
#[test]
fn an_agent_in_the_root_group_or_on_cgroup_v1_has_no_group_of_its_own() {
    for own in ["1:name=systemd:/system.slice/agent.service\n", "0::/\n"] {
        let host = FakeHost::quiet();
        host.set_file("/proc/self/cgroup", own);
        let mut checks = started(&host, vec![]);
        host.start_process(process(300, 1, 0, 900, "0::/system.slice/agent.service"));
        assert!(after(&mut checks, "r1").is_ok(), "{own:?}");
        host.start_process(process(301, 1, 0, 901, "0::/"));
        let found = after(&mut checks, "r2").unwrap_err();
        assert_eq!(found.code(), code::DAEMONISED_CHILD, "{own:?}");
    }
}
