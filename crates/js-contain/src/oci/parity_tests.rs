//! The resolved bundle of a job against what the engine this backend replaces gave it:
//! no capability, no new privileges, a `/tmp` that cannot run programs, no core dumps,
//! `nosuid` and `nodev` on every bind. Each test reads the `config.json` the runner
//! writes (`spec::render`), not the code that builds it, so a setting dropped or
//! weakened anywhere on the way is seen. What a real `crun` does with the bundle is for
//! the real-`crun` tests.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::spec::{Placement, SpecError, action_config, render};
use crate::container::tests::spec;
use crate::container::{ContainerSpec, DeviceNode, HostMount, NPROC_HIGH};
use crate::subids::DaemonIds;

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
        route_wait_secs: 0,
    };
    let config = action_config(spec, &placement)?;
    Ok(serde_json::from_slice(&render(&config).expect("render")).expect("json"))
}

fn config() -> Value {
    config_with(&spec()).expect("config")
}

fn options(mount: &Value) -> Vec<&str> {
    mount["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

fn mount_at<'a>(config: &'a Value, destination: &str) -> &'a Value {
    config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|m| m["destination"] == destination)
        .unwrap_or_else(|| panic!("no mount at {destination}"))
}

/// Catches a job that keeps a capability (podman ran every job with `--cap-drop ALL`): all
/// five sets of `process.capabilities` are present and empty, so `crun` has nothing to
/// fall back to a default for.
#[test]
fn every_capability_set_is_empty() {
    let config = config();
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
        assert_eq!(set, &json!([]), "{name}");
    }
}

/// Catches a job that can gain privilege through a set-uid or file-capability program
/// (podman's `--security-opt no-new-privileges`).
#[test]
fn no_new_privileges_is_set() {
    assert_eq!(config()["process"]["noNewPrivileges"], true);
}

/// Catches a `/tmp` a job can run programs from, with no size, or one that is still the
/// lease's disk directory: it is a tmpfs of the limits' size, `noexec`, `nosuid`, `nodev`
/// and world-writable with the sticky bit (podman's
/// `--tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m`).
#[test]
fn tmp_is_a_noexec_tmpfs_with_a_size() {
    let config = config();
    let tmp = mount_at(&config, "/tmp");
    assert_eq!(tmp["type"], "tmpfs");
    let options = options(tmp);
    for want in ["noexec", "nosuid", "nodev", "mode=1777", "size=106m"] {
        assert!(options.contains(&want), "{want} not in {options:?}");
    }
    let tmps = config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .filter(|m| m["destination"] == "/tmp")
        .count();
    assert_eq!(tmps, 1, "one mount at /tmp");
}

/// Catches `/var/tmp` or `/run` left as a place a job can run programs from, or with no
/// size (podman made both `rw,noexec,nosuid,nodev,size=64m` tmpfs mounts, as `/tmp`): each
/// is one tmpfs mount of the limits' size, `noexec`, `nosuid`, `nodev`, mode 1777, and not
/// also a bind of a lease directory.
#[test]
fn var_tmp_and_run_are_noexec_tmpfs_with_a_size() {
    let config = config();
    for dest in ["/var/tmp", "/run"] {
        let mount = mount_at(&config, dest);
        assert_eq!(mount["type"], "tmpfs", "{dest}");
        let options = options(mount);
        for want in ["noexec", "nosuid", "nodev", "mode=1777", "size=106m"] {
            assert!(options.contains(&want), "{dest}: {want} not in {options:?}");
        }
        let count = config["mounts"]
            .as_array()
            .expect("mounts")
            .iter()
            .filter(|m| m["destination"] == dest)
            .count();
        assert_eq!(count, 1, "one mount at {dest}");
    }
}

/// Catches core dumps left on, and the other two limits lost with them: `RLIMIT_CORE` is
/// 0 soft and hard (`--ulimit core=0`), and the file and process limits are the
/// limits' own.
#[test]
fn core_dumps_are_off_and_the_other_limits_stay() {
    let config = config();
    let rlimits = config["process"]["rlimits"].as_array().expect("rlimits");
    let of = |name: &str| {
        rlimits
            .iter()
            .find(|r| r["type"] == name)
            .unwrap_or_else(|| panic!("no {name}"))
    };
    assert_eq!(
        of("RLIMIT_CORE"),
        &json!({"type": "RLIMIT_CORE", "hard": 0, "soft": 0})
    );
    assert_eq!(of("RLIMIT_NOFILE")["hard"], 103);
    assert_eq!(of("RLIMIT_NPROC")["hard"], 104);
    assert_eq!(rlimits.len(), 3);
}

/// Catches a bind a job could use to run a set-uid file or open a device node: every
/// mount of type `bind` in the bundle, whoever added it (the init, the name files, the
/// shared trees, the writable directories, the host mounts), lists `nosuid` and `nodev`.
#[test]
fn every_bind_is_nosuid_and_nodev() {
    let mut spec = spec();
    spec.shared = vec![crate::container::SharedMount {
        path: "lib".to_owned(),
        source: PathBuf::from("/scratch/trees/bb"),
    }];
    let config = config_with(&spec).expect("config");
    let binds: Vec<&Value> = config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .filter(|m| m["type"] == "bind")
        .collect();
    // init, 3 name files, 1 shared, the home directory, 3 host mounts.
    assert_eq!(binds.len(), 9, "{binds:?}");
    for bind in binds {
        let options = options(bind);
        assert!(options.contains(&"nosuid"), "{bind}");
        assert!(options.contains(&"nodev"), "{bind}");
    }
}

/// Catches the run's scratch directory, a `--mount-root` mount or the run's files bound
/// at the wrong place, with the wrong access, or before the lease's own mounts (which
/// would hide a mount the host named): each is a `rbind` of its source at its target,
/// `rw` or `ro` as asked, after the writable lease directories.
#[test]
fn host_mounts_are_binds_at_their_targets_with_their_access() {
    let config = config();
    let mounts = config["mounts"].as_array().expect("mounts");
    let at = |dest: &str| mounts.iter().position(|m| m["destination"] == dest);
    let last_writable = at("/root").expect("/root");
    for (target, source, access, other) in [
        ("/scratch", "/scratch/lease-1-2/job", "rw", "ro"),
        ("/workspace", "/srv/mount-root", "rw", "ro"),
        ("/run/js-files", "/run/js-agent/files/1-2", "ro", "rw"),
    ] {
        let position = at(target).unwrap_or_else(|| panic!("no mount at {target}"));
        assert!(last_writable < position, "{target} before the lease's own");
        let mount = &mounts[position];
        assert_eq!(mount["type"], "bind");
        assert_eq!(mount["source"], source);
        let options = options(mount);
        assert!(options.contains(&"rbind"), "{target}: {options:?}");
        assert!(options.contains(&access), "{target}: {options:?}");
        assert!(!options.contains(&other), "{target}: {options:?}");
    }
}

/// Catches a host mount whose target could name another place than it says (relative,
/// `/`, `.` or `..` parts, a trailing slash), as `exec_root` is refused.
#[test]
fn host_mount_targets_must_be_normalized_absolute_paths() {
    for bad in ["", "/", "work", "/a/../b", "/a/./b", "/a//b", "/a/"] {
        let mut spec = spec();
        spec.mounts = vec![HostMount {
            target: bad.to_owned(),
            source: PathBuf::from("/srv/x"),
            read_only: false,
            recursive: true,
        }];
        let why = config_with(&spec).expect_err(bad).to_string();
        assert!(why.starts_with("mount target "), "{bad}: {why}");
    }
}

/// Catches the filter still resolved for the old capability set: with none held,
/// `chroot` (allowed only for `CAP_SYS_CHROOT`) is refused with an errno, as under
/// podman's `--cap-drop ALL`, and is not also allowed by another entry.
#[test]
fn the_seccomp_filter_is_resolved_for_no_capabilities() {
    let config = config();
    let calls = config["linux"]["seccomp"]["syscalls"]
        .as_array()
        .expect("syscalls");
    let action_of = |name: &str| -> Vec<&str> {
        calls
            .iter()
            .filter(|c| {
                c["names"]
                    .as_array()
                    .is_some_and(|n| n.iter().any(|n| n == name))
            })
            .filter_map(|c| c["action"].as_str())
            .collect()
    };
    assert_eq!(action_of("chroot"), ["SCMP_ACT_ERRNO"]);
}

/// Catches the default process limit left where one job's forks could fail another's
/// (the kernel counts it per user and every job is the same subordinate id): the
/// default is high enough that the lease's `pids.max` is the bound, and a caller's
/// choice is kept.
#[test]
fn the_default_process_limit_is_high_so_pids_max_is_the_bound() {
    assert_eq!(NPROC_HIGH, 4_194_304);
    assert_eq!(crate::container::ContainerLimits::DEFAULT.nproc, NPROC_HIGH);
}

/// Catches a process limit above the daemon's own hard limit reaching the bundle: a
/// rootless daemon cannot raise a hard limit (`setrlimit` needs `CAP_SYS_RESOURCE` in the
/// initial user namespace), so `crun`'s `setrlimit` would fail and the container never
/// start. The limit is cut to the ceiling, and kept when below it.
#[test]
fn the_process_limit_never_exceeds_what_the_daemon_may_give() {
    let limits = crate::container::ContainerLimits::DEFAULT;
    assert_eq!(limits.within_nproc(1000).nproc, 1000);
    assert_eq!(limits.within_nproc(u64::MAX).nproc, NPROC_HIGH);
    let mut low = limits;
    low.nproc = 10;
    assert_eq!(low.within_nproc(1000).nproc, 10);
    // Everything else is untouched.
    let cut = limits.within_nproc(1000);
    assert_eq!(
        (cut.pids, cut.nofile, cut.tmp_mib),
        (limits.pids, limits.nofile, limits.tmp_mib)
    );
}

/// Catches the daemon's ceiling read from the wrong place: it is the hard `Max processes`
/// of this process, as `/proc/self/limits` prints it (`unlimited` is the largest value).
#[test]
fn the_ceiling_is_the_daemons_hard_process_limit() {
    let limits = std::fs::read_to_string("/proc/self/limits").expect("limits");
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max processes"))
        .expect("Max processes");
    let hard = line.split_whitespace().nth(3).expect("hard column");
    let want = if hard == "unlimited" {
        u64::MAX
    } else {
        hard.parse().expect("number")
    };
    assert_eq!(crate::container::own_nproc_hard_limit(), want);
}

/// The node `/dev/<name>` of device `major`:`minor`, owned by `uid`:`gid`, mode `mode`.
fn node(name: &str, (major, minor): (u32, u32), (uid, gid): (u32, u32), mode: u32) -> DeviceNode {
    DeviceNode {
        path: format!("/dev/{name}"),
        major,
        minor,
        uid,
        gid,
        mode,
    }
}

/// The reference job given the GPU nodes a run that booked indices 1 and 3 is given.
fn gpu_job() -> ContainerSpec {
    let mut spec = spec();
    spec.devices = vec![
        node("nvidiactl", (195, 255), (0, 0), 0o666),
        node("nvidia1", (195, 1), (0, 44), 0o660),
        node("nvidia3", (195, 3), (0, 44), 0o660),
    ];
    spec
}

/// Proves a container given device nodes gets exactly those, each a character device at its
/// own path with its numbers and owner, and a device cgroup that denies every device and then
/// allows each of them, and nothing else, to be read and written (podman's `--device`, with
/// the default deny of its device rules). Catches a container that is given another node than
/// its run booked, a rule that allows a whole major number (every GPU), one that allows
/// `mknod`, and a list that does not deny first.
#[test]
fn a_container_given_devices_gets_their_nodes_and_rules_that_allow_only_them() {
    let config = config_with(&gpu_job()).expect("config");
    assert_eq!(
        config["linux"]["devices"],
        json!([
            {"path": "/dev/nvidiactl", "type": "c", "major": 195, "minor": 255,
             "fileMode": 0o666, "uid": 0, "gid": 0},
            {"path": "/dev/nvidia1", "type": "c", "major": 195, "minor": 1,
             "fileMode": 0o660, "uid": 0, "gid": 44},
            {"path": "/dev/nvidia3", "type": "c", "major": 195, "minor": 3,
             "fileMode": 0o660, "uid": 0, "gid": 44},
        ])
    );
    let resources = &config["linux"]["resources"];
    assert_eq!(
        resources["devices"],
        json!([
            {"allow": false, "access": "rwm"},
            {"allow": true, "type": "c", "major": 195, "minor": 255, "access": "rw"},
            {"allow": true, "type": "c", "major": 195, "minor": 1, "access": "rw"},
            {"allow": true, "type": "c", "major": 195, "minor": 3, "access": "rw"},
        ])
    );
    // The other limits of the cgroup are there beside the rules.
    assert_eq!(resources["pids"], json!({"limit": 101}));
    assert_eq!(resources["unified"], json!({"memory.oom.group": "1"}));
}

/// Proves a device node is given at most `0666`, and a node whose own mode is narrower keeps
/// its own: the set-id and sticky bits and execute bits are never given. Catches a node
/// handed to the job wider than the host's, and one given wider than read and write.
#[test]
fn a_device_node_is_given_at_most_read_and_write() {
    let mut spec = spec();
    spec.devices = [0o7777, 0o666, 0o640, 0o600, 0o000]
        .iter()
        .enumerate()
        .map(|(n, mode)| node(&format!("d{n}"), (10, 200), (0, 0), *mode))
        .collect();
    let config = config_with(&spec).expect("config");
    let modes: Vec<u64> = config["linux"]["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|device| device["fileMode"].as_u64().expect("a mode"))
        .collect();
    assert_eq!(modes, [0o666, 0o666, 0o640, 0o600, 0o000]);
}

/// Proves a container given no device has no device section at all (neither nodes nor
/// rules), so the bundle of every job without a GPU is what it was before devices were
/// given. Catches a device rule list written for every container.
#[test]
fn a_container_given_no_device_has_no_device_section() {
    let config = config();
    assert!(config["linux"].get("devices").is_none(), "{config}");
    assert!(
        config["linux"]["resources"].get("devices").is_none(),
        "{config}"
    );
}

/// Proves a device whose path is not a normalized path below `/dev` is refused, naming the
/// device path: a node made outside the container's own `/dev` tmpfs would be made in the
/// image's root, which every lease of the image shares. Catches a relative path, one that
/// climbs out with `..`, `/dev` itself and a path elsewhere being written into the bundle.
#[test]
fn a_device_path_outside_dev_is_refused() {
    for bad in [
        "/tmp/nvidia0",
        "dev/nvidia0",
        "/dev/../etc/x",
        "/dev/",
        "/dev",
        "/dev//x",
    ] {
        let mut spec = gpu_job();
        spec.devices[1].path = bad.to_owned();
        let why = config_with(&spec).expect_err(bad).to_string();
        assert!(
            why.starts_with(&format!("device path {bad:?} ")),
            "{bad}: {why}"
        );
    }
}

/// Proves the GPU's nodes give the job no capability: with devices, every capability set is
/// still empty and new privileges are still refused, so a job cannot remount what it is given
/// (`CAP_SYS_ADMIN`) or make another node (`CAP_MKNOD`). Catches device support that widens
/// the capability set.
#[test]
fn a_container_given_devices_still_holds_no_capability() {
    let config = config_with(&gpu_job()).expect("config");
    let sets = config["process"]["capabilities"].as_object().expect("sets");
    assert_eq!(sets.len(), 5);
    for (name, set) in sets {
        assert_eq!(set, &json!([]), "{name}");
    }
    assert_eq!(config["process"]["noNewPrivileges"], true);
}

/// Proves a host mount that is not recursive is a plain `bind` with the same flags, and a
/// recursive one an `rbind`: no mount below a non-recursive mount's directory on the host
/// reaches the job, whatever that mount's own flags. Catches a driver directory bound with
/// its submounts, where a writable one below it would be writable in the job.
#[test]
fn a_mount_that_is_not_recursive_is_a_plain_bind() {
    let mut spec = spec();
    spec.mounts.push(HostMount {
        target: "/usr/local/driver".to_owned(),
        source: PathBuf::from("/opt/driver"),
        read_only: true,
        recursive: false,
    });
    let config = config_with(&spec).expect("config");
    assert_eq!(
        options(mount_at(&config, "/usr/local/driver")),
        ["bind", "ro", "nosuid", "nodev"]
    );
    assert_eq!(
        options(mount_at(&config, "/run/js-files")),
        ["rbind", "ro", "nosuid", "nodev"]
    );
}
