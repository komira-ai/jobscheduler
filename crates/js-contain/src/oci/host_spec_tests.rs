//! The `config.json` of a host container ([`Isolation::Host`]): what differs from a
//! contained one's, what stays the same, and what a host container is refused.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::host_spec::HOST_CAPABILITIES;
use super::spec::{Placement, SpecError, action_config, render};
use crate::container::tests::spec as contained;
use crate::container::{ContainerSpec, DeviceNode, HostMount, Isolation, SharedMount};
use crate::subids::DaemonIds;

/// The reference job as a host container: the run's scratch directory as its exec root,
/// and none of the mounts a contained job is given.
fn host() -> ContainerSpec {
    ContainerSpec {
        isolation: Isolation::Host,
        exec_root: "/scratch/runs/r1".to_owned(),
        working_directory: String::new(),
        mounts: Vec::new(),
        ..contained()
    }
}

/// The config of `spec`, placed as the contained tests place theirs.
fn config_with(spec: &ContainerSpec) -> Result<Value, SpecError> {
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
    let placement = Placement {
        rootfs: &rootfs,
        cgroup: "/js.slice/actions/lease-1-2",
        ids: &ids,
        arch: "amd64",
        init: Path::new("/scratch/oci-bin/js-init"),
        init_dest: "/dev/js-init",
        route_wait_secs: 7,
    };
    let config = action_config(spec, &placement)?;
    Ok(serde_json::from_slice(&render(&config).expect("render")).expect("json"))
}

fn config_of(spec: &ContainerSpec) -> Value {
    config_with(spec).expect("config")
}

/// Proves the whole `config.json` of a host container, as a file: the host's `/` as its
/// root, no namespace, no id mapping, no mount, root with every capability, no seccomp
/// filter, no masked path, no hostname, and the init at its host path; the cgroup, the pids
/// limit, the OOM group and the rlimits as a contained job's. Any field added, dropped or
/// changed is a diff to read. Regenerate by replacing the file with the actual text the
/// failure prints.
#[test]
fn the_host_config_matches_its_golden_file() {
    let actual = serde_json::to_string_pretty(&config_of(&host())).expect("json") + "\n";
    let golden = include_str!("../../tests/fixtures/oci-host-config.golden.json");
    assert!(
        actual == golden,
        "config.json differs from tests/fixtures/oci-host-config.golden.json; actual:\n{actual}"
    );
}

/// Proves a host container shares every namespace of the host and maps no id: the
/// namespace list is written out empty (`oci-spec`'s default list would give it new ones)
/// and there is no `uidMappings` or `gidMappings`. Catches a host job left in a pid or
/// network namespace of its own, or in a user namespace that makes its root nobody's.
#[test]
fn a_host_container_has_no_namespace_and_no_id_mapping() {
    let config = config_of(&host());
    assert_eq!(config["linux"]["namespaces"], json!([]));
    assert!(config["linux"].get("uidMappings").is_none());
    assert!(config["linux"].get("gidMappings").is_none());
    assert_eq!(config["root"], json!({"path": "/", "readonly": false}));
    assert_eq!(config["process"]["user"], json!({"uid": 0, "gid": 0}));
}

/// Proves a host container touches nothing of the host's mounts or name: no mount at all
/// (a `/proc`, `/dev` or `/tmp` mount would cover the host's own in the shared mount
/// namespace), no masked or read-only path, and no hostname (setting it would rename the
/// host). Catches the contained job's lists leaking into the host config.
#[test]
fn a_host_container_mounts_masks_and_renames_nothing() {
    let config = config_of(&host());
    assert_eq!(config["mounts"], json!([]));
    assert_eq!(config["linux"]["maskedPaths"], json!([]));
    assert_eq!(config["linux"]["readonlyPaths"], json!([]));
    assert!(config.get("hostname").is_none());
    assert!(config.get("annotations").is_none());
    assert!(config["linux"].get("seccomp").is_none());
}

/// Proves a host job holds every capability in all five sets and may gain privileges (a
/// set-uid program works), and that each name is one `oci-spec` knows: a name it did not
/// know would be dropped from the set without a word. Catches a host job that cannot do
/// what it is for (load a module, install a package).
#[test]
fn a_host_job_holds_every_capability() {
    let config = config_of(&host());
    let capabilities = &config["process"]["capabilities"];
    for set in [
        "bounding",
        "effective",
        "permitted",
        "inheritable",
        "ambient",
    ] {
        let held: Vec<&str> = capabilities[set]
            .as_array()
            .expect(set)
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(held, HOST_CAPABILITIES, "{set}");
    }
    assert_eq!(config["process"]["noNewPrivileges"], false);
}

/// Proves a host container keeps what accounts and ends it: its cgroup is `ctr` below the
/// lease's, with the pids limit and `memory.oom.group`, no memory or CPU limit of its own
/// (the lease's apply), and the same rlimits as a contained job. Catches a host job placed
/// outside its lease's cgroup, where the lease's limits and `cgroup.kill` do not reach it.
#[test]
fn a_host_container_is_in_its_leases_cgroup_with_the_contained_limits() {
    let host = config_of(&host());
    let contained = config_of(&contained());
    assert_eq!(
        host["linux"]["cgroupsPath"],
        "/js.slice/actions/lease-1-2/ctr"
    );
    assert_eq!(
        host["linux"]["cgroupsPath"],
        contained["linux"]["cgroupsPath"]
    );
    let resources = &host["linux"]["resources"];
    assert_eq!(
        resources,
        &json!({"pids": {"limit": 101}, "unified": {"memory.oom.group": "1"}})
    );
    assert_eq!(host["process"]["rlimits"], contained["process"]["rlimits"]);
    assert_eq!(host["process"]["oomScoreAdj"], 0);
}

/// Proves a host job starts in its exec root (the run's scratch directory on the host) as
/// the init's child, with the init at its path on the host and no route wait (the host's
/// network is there), whatever working directory the spec names. Catches a host job run
/// from the image's directories, or held for a route nobody adds.
#[test]
fn a_host_job_runs_under_the_init_at_its_host_path_in_its_exec_root() {
    let mut spec = host();
    spec.working_directory = "pkg".to_owned();
    let config = config_of(&spec);
    assert_eq!(config["process"]["cwd"], "/scratch/runs/r1");
    assert_eq!(
        config["process"]["args"],
        json!(["/scratch/oci-bin/js-init", "--", "tool", "--flag", "a b"])
    );
}

/// Proves a host job's environment is the Command's, then `HOME=/root` unless it sets one,
/// and never a `HOSTNAME` (the host's name stands). Catches the contained defaults, which
/// name every container `localhost`.
#[test]
fn a_host_jobs_environment_is_the_commands_and_a_home() {
    let mut spec = host();
    assert_eq!(
        config_of(&spec)["process"]["env"],
        json!(["PATH=/bin", "HOME=/root"])
    );
    spec.env.push(("HOME".to_owned(), "/home/build".to_owned()));
    assert_eq!(
        config_of(&spec)["process"]["env"],
        json!(["PATH=/bin", "HOME=/home/build"])
    );
}

/// Proves a host container given a mount, a shared tree or a device is refused by name
/// (`SpecError::HostMounts`), each alone: the job sees the host's own, and none could be
/// given without changing the host's mount namespace or device rules. Catches a spec that
/// would bind over a host path.
#[test]
fn a_host_container_with_a_mount_a_tree_or_a_device_is_refused() {
    let mut mounted = host();
    mounted.mounts.push(HostMount {
        target: "/workspace".to_owned(),
        source: PathBuf::from("/srv/mount-root"),
        read_only: true,
        recursive: true,
    });
    let mut shared = host();
    shared.shared.push(SharedMount {
        path: "lib".to_owned(),
        source: PathBuf::from("/scratch/trees/bb"),
    });
    let mut device = host();
    device.devices.push(DeviceNode {
        path: "/dev/nvidia0".to_owned(),
        major: 195,
        minor: 0,
        uid: 0,
        gid: 0,
        mode: 0o666,
    });
    for (what, spec) in [("mount", mounted), ("tree", shared), ("device", device)] {
        let refused = config_with(&spec).expect_err(what);
        assert!(
            matches!(refused, SpecError::HostMounts),
            "{what}: {refused}"
        );
        assert_eq!(
            refused.to_string(),
            "a host container is given no mount, shared tree or device"
        );
    }
}

/// Proves a host container's exec root and init path must be normalized absolute paths:
/// a relative, `..` or trailing-slash one is refused by the setting's name. Catches a
/// working directory or init that names another place than it says.
#[test]
fn a_host_containers_exec_root_and_init_must_be_normalized() {
    for bad in ["", "/", "runs/r1", "/a/../b", "/a/"] {
        let mut spec = host();
        spec.exec_root = bad.to_owned();
        let why = config_with(&spec).expect_err(bad).to_string();
        assert!(why.starts_with("exec_root "), "{bad}: {why}");
    }
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let rootfs = PathBuf::from("/");
    let placement = Placement {
        rootfs: &rootfs,
        cgroup: "/js.slice/actions/lease-1-2",
        ids: &ids,
        arch: "amd64",
        init: Path::new("oci-bin/js-init"),
        init_dest: "/dev/js-init",
        route_wait_secs: 0,
    };
    let why = action_config(&host(), &placement).expect_err("relative init");
    assert!(why.to_string().starts_with("init path "), "{why}");
}

/// Proves who `crun` runs as for a lease: the job user for a root daemon that drops to one,
/// unless the lease's directory is the daemon's own (a host lease's), and the daemon
/// itself when it drops to nobody; a lease whose directory is missing is the job user's.
/// Catches a host container's `crun` run as the job user (it cannot make a container with
/// no user namespace), and a contained lease's run as the daemon.
#[test]
fn crun_runs_as_the_job_user_unless_the_lease_is_the_daemons() {
    use super::runs_as;
    assert_eq!(runs_as(Some(1500), 0, Some(1500)), 1500);
    assert_eq!(runs_as(Some(1500), 0, None), 1500);
    assert_eq!(runs_as(Some(1500), 0, Some(0)), 0);
    assert_eq!(runs_as(None, 1000, Some(1500)), 1000);
    assert_eq!(runs_as(None, 1000, None), 1000);
}
