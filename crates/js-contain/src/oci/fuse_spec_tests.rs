//! The `config.json` of a container given the FUSE device ([`ContainerSpec::fuse`]):
//! `CAP_SYS_ADMIN` of its own user namespace, `/dev/fuse`, and a seccomp filter resolved for
//! that capability with the `io_uring` calls allowed; everything else as a contained job's.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::spec::{Placement, SpecError, action_config, render};
use crate::container::tests::spec as plain;
use crate::container::{ContainerSpec, Isolation};
use crate::subids::DaemonIds;

/// The reference job given the FUSE device.
fn fuse_job() -> ContainerSpec {
    ContainerSpec {
        fuse: true,
        ..plain()
    }
}

/// The config of `spec` for architecture `arch`.
fn config_on(spec: &ContainerSpec, arch: &str) -> Result<Value, SpecError> {
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
        arch,
        init: Path::new("/scratch/oci-bin/js-init"),
        init_dest: "/dev/js-init",
        route_wait_secs: 0,
    };
    let config = action_config(spec, &placement)?;
    Ok(serde_json::from_slice(&render(&config).expect("render")).expect("json"))
}

fn config_of(spec: &ContainerSpec) -> Value {
    config_on(spec, "amd64").expect("config")
}

/// The syscall names the config's filter allows (an entry that allows them, whatever its
/// arguments).
fn allowed(config: &Value) -> BTreeSet<String> {
    config["linux"]["seccomp"]["syscalls"]
        .as_array()
        .expect("syscalls")
        .iter()
        .filter(|entry| entry["action"] == "SCMP_ACT_ALLOW")
        .flat_map(|entry| entry["names"].as_array().expect("names").iter())
        .map(|name| name.as_str().expect("a name").to_owned())
        .collect()
}

/// Proves a job given the FUSE device holds `CAP_SYS_ADMIN` and nothing else in each of the
/// five capability sets (the ambient set too: the process is not root), and keeps
/// `noNewPrivileges`, so a set-uid program gains nothing. Catches a set that is wider than
/// the one capability, one left out (a non-root process then has none), and new privileges
/// allowed.
#[test]
fn a_fuse_job_holds_cap_sys_admin_alone_in_all_five_sets() {
    let config = config_of(&fuse_job());
    let sets = config["process"]["capabilities"].as_object().expect("sets");
    let mut names: Vec<&str> = sets.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "ambient",
            "bounding",
            "effective",
            "inheritable",
            "permitted"
        ]
    );
    for (name, set) in sets {
        assert_eq!(set, &json!(["CAP_SYS_ADMIN"]), "{name}");
    }
    assert_eq!(config["process"]["noNewPrivileges"], true);
}

/// Proves a job given the FUSE device is given `/dev/fuse` as character device 10:229 with
/// mode 0666 owned by root, and a device cgroup that denies every device and then allows
/// that node to be read and written, as it does a GPU's. Catches a missing node, a wrong
/// number, a rule that allows `mknod`, and a list that does not deny first.
#[test]
fn a_fuse_job_is_given_dev_fuse_and_a_rule_that_allows_only_it() {
    let config = config_of(&fuse_job());
    assert_eq!(
        config["linux"]["devices"],
        json!([
            {"path": "/dev/fuse", "type": "c", "major": 10, "minor": 229,
             "fileMode": 0o666, "uid": 0, "gid": 0},
        ])
    );
    assert_eq!(
        config["linux"]["resources"]["devices"],
        json!([
            {"allow": false, "access": "rwm"},
            {"allow": true, "type": "c", "major": 10, "minor": 229, "access": "rw"},
        ])
    );
}

/// Proves a FUSE job booked GPUs keeps them: its nodes are the GPUs' then `/dev/fuse`, and
/// the rules allow each. Catches the FUSE node replacing the booked ones.
#[test]
fn a_fuse_job_with_gpus_is_given_them_and_dev_fuse() {
    let mut spec = fuse_job();
    spec.devices = vec![crate::container::DeviceNode {
        path: "/dev/nvidia0".to_owned(),
        major: 195,
        minor: 0,
        uid: 0,
        gid: 44,
        mode: 0o660,
    }];
    let config = config_of(&spec);
    let paths: Vec<&str> = config["linux"]["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .filter_map(|device| device["path"].as_str())
        .collect();
    assert_eq!(paths, ["/dev/nvidia0", "/dev/fuse"]);
    let rules = config["linux"]["resources"]["devices"]
        .as_array()
        .expect("rules");
    assert_eq!(rules.len(), 3);
}

/// Proves a FUSE job is otherwise a contained job's: the same namespaces (a network
/// namespace of its own, nothing configured in it, and a user namespace), the same id
/// mappings, masked and read-only paths, mounts, rlimits and cgroup. Catches the device
/// widening the sandbox beyond what the design gives it.
#[test]
fn a_fuse_job_keeps_the_namespaces_paths_and_mounts_of_a_contained_job() {
    let fuse = config_of(&fuse_job());
    let plain = config_of(&plain());
    let namespaces: Vec<&str> = fuse["linux"]["namespaces"]
        .as_array()
        .expect("namespaces")
        .iter()
        .filter_map(|n| n["type"].as_str())
        .collect();
    assert_eq!(
        namespaces,
        ["pid", "network", "ipc", "uts", "user", "cgroup", "mount"]
    );
    for part in [
        &["linux", "namespaces"][..],
        &["linux", "uidMappings"],
        &["linux", "gidMappings"],
        &["linux", "maskedPaths"],
        &["linux", "readonlyPaths"],
        &["linux", "cgroupsPath"],
        &["mounts"],
        &["hostname"],
        &["root"],
        &["process", "user"],
        &["process", "rlimits"],
        &["process", "args"],
    ] {
        let at = |config: &Value| part.iter().fold(config.clone(), |v, key| v[*key].clone());
        assert_eq!(at(&fuse), at(&plain), "{part:?}");
    }
    assert_eq!(
        fuse["linux"]["resources"]["pids"],
        plain["linux"]["resources"]["pids"]
    );
}

/// Proves the filter of a FUSE job is the default job's with exactly the calls
/// `CAP_SYS_ADMIN` opens in the pinned profile (`bpf`, `fanotify_init`, `lookup_dcookie`,
/// `perf_event_open`, `quotactl`, `setdomainname`, `sethostname`) and the three `io_uring`
/// calls added, and nothing the default allows taken away; on both architectures. Catches
/// an `io_uring` call left refused, one more call opened, and a filter resolved for the
/// wrong capability set.
#[test]
fn the_fuse_filter_is_the_defaults_plus_the_admin_calls_and_io_uring() {
    for arch in ["amd64", "arm64"] {
        let default = allowed(&config_on(&plain(), arch).expect("default"));
        let fuse = allowed(&config_on(&fuse_job(), arch).expect("fuse"));
        let gained: Vec<&String> = fuse.difference(&default).collect();
        assert_eq!(
            gained,
            [
                "bpf",
                "fanotify_init",
                "io_uring_enter",
                "io_uring_register",
                "io_uring_setup",
                "lookup_dcookie",
                "perf_event_open",
                "quotactl",
                "setdomainname",
                "sethostname",
            ],
            "{arch}"
        );
        let lost: Vec<&String> = default.difference(&fuse).collect();
        assert!(lost.is_empty(), "{arch}: {lost:?}");
    }
}

/// Proves a config with no FUSE device has no `io_uring` call allowed and no `/dev/fuse`:
/// the default is as it was. Catches the extra calls leaking into every job's filter.
#[test]
fn a_job_not_given_fuse_has_no_io_uring_and_no_dev_fuse() {
    let config = config_of(&plain());
    let default = allowed(&config);
    assert!(
        !default.iter().any(|name| name.starts_with("io_uring")),
        "{default:?}"
    );
    assert!(config["linux"].get("devices").is_none());
}

/// Proves a FUSE job on an architecture the profile has no filter for is refused with the
/// seccomp error, as any job is.
#[test]
fn a_fuse_job_on_an_unknown_architecture_is_refused() {
    let refused = config_on(&fuse_job(), "riscv64").expect_err("no filter");
    assert_eq!(
        refused.to_string(),
        "the seccomp profile has no filter for architecture \"riscv64\""
    );
}

/// Proves a host container asked for the FUSE device is refused by name: it sees the host's
/// own `/dev`. Catches a host job silently run without the device it was told it has.
#[test]
fn a_host_container_given_fuse_is_refused() {
    let spec = ContainerSpec {
        isolation: Isolation::Host,
        exec_root: "/scratch/runs/r1".to_owned(),
        working_directory: String::new(),
        mounts: Vec::new(),
        fuse: true,
        ..plain()
    };
    let refused = config_on(&spec, "amd64").expect_err("refused");
    assert!(matches!(refused, SpecError::HostFuse), "{refused}");
    assert_eq!(
        refused.to_string(),
        "a host container is not given the FUSE device: it sees the host's own"
    );
}

/// The whole `config.json` of the reference job given the FUSE device, as a file, as the
/// contained job's is (the seccomp entries counted). Regenerate by replacing the file with
/// the actual text the failure prints.
#[test]
fn the_fuse_config_matches_its_golden_file() {
    let mut config = config_of(&fuse_job());
    let seccomp = &mut config["linux"]["seccomp"];
    let entries = seccomp["syscalls"].as_array().expect("syscalls").len();
    seccomp["syscalls"] = json!(format!("{entries} entries"));
    let actual = serde_json::to_string_pretty(&config).expect("json") + "\n";
    let golden = include_str!("../../tests/fixtures/oci-action-config-fuse.golden.json");
    assert!(
        actual == golden,
        "config.json differs from tests/fixtures/oci-action-config-fuse.golden.json; actual:\n{actual}"
    );
}
