//! What `config.json` says for a job's own file system: a root the caller mounted, a host
//! mount that asks for `noexec`, and a host mount at the exec root in the place of its
//! overlay ([`crate::job_fs`] makes the mounts these name).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::spec::{Placement, action_config, render};
use crate::container::tests::spec;
use crate::container::{ContainerSpec, HostMount};
use crate::subids::DaemonIds;

/// The store's tree of the reference job's image.
const IMAGE_ROOTFS: &str = "/scratch/oci-images/aaaa/rootfs";

fn config_of(spec: &ContainerSpec) -> Value {
    let rootfs = PathBuf::from(IMAGE_ROOTFS);
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let placement = Placement {
        rootfs: &rootfs,
        cgroup: "/js.slice/actions/lease-1-2",
        ids: &ids,
        arch: "amd64",
        init: Path::new("/scratch/oci-bin/js-init"),
        init_dest: "/dev/js-init",
        route_wait_secs: 0,
    };
    let config = action_config(spec, &placement).expect("config");
    serde_json::from_slice(&render(&config).expect("render")).expect("json")
}

fn host(target: &str, source: &str, read_only: bool, no_exec: bool) -> HostMount {
    HostMount {
        target: target.to_owned(),
        source: PathBuf::from(source),
        read_only,
        recursive: true,
        no_exec,
    }
}

/// The mount at `destination`, and where it is in the list.
fn mount_at<'a>(config: &'a Value, destination: &str) -> Option<(usize, &'a Value)> {
    config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .enumerate()
        .find(|(_, m)| m["destination"] == destination)
}

/// Catches a mounted root given to `crun` read-only (the job could write nothing of its
/// root), or the image's tree used in its place, and a job without one losing its
/// read-only image root.
#[test]
fn a_mounted_root_is_the_containers_writable_root() {
    let mut layered = spec();
    layered.mounted_root = Some(PathBuf::from("/run/js-agent/mounts/lease-1-2/rootfs"));
    assert_eq!(
        config_of(&layered)["root"],
        json!({"path": "/run/js-agent/mounts/lease-1-2/rootfs", "readonly": false})
    );
    assert_eq!(
        config_of(&spec())["root"],
        json!({"path": IMAGE_ROOTFS, "readonly": true})
    );
}

/// Catches `noexec` dropped from a host mount that asks for it, or given to one that does
/// not, and a host mount losing `nosuid` or `nodev` either way.
#[test]
fn a_host_mount_is_noexec_only_when_it_asks() {
    let mut job = spec();
    job.mounts = vec![
        host("/data/in", "/m/l/src/0", true, true),
        host("/data/out", "/m/l/src/1", false, true),
        host("/data/tools", "/m/l/src/2", true, false),
    ];
    let config = config_of(&job);
    for (destination, options) in [
        (
            "/data/in",
            json!(["rbind", "ro", "nosuid", "nodev", "noexec"]),
        ),
        (
            "/data/out",
            json!(["rbind", "rw", "nosuid", "nodev", "noexec"]),
        ),
        ("/data/tools", json!(["rbind", "ro", "nosuid", "nodev"])),
    ] {
        let (_, mount) = mount_at(&config, destination).expect(destination);
        assert_eq!(mount["options"], options, "{destination}");
        assert_eq!(mount["type"], "bind");
    }
}

/// Catches a host mount at the exec root bound over the exec root's overlay (two mounts
/// where one is asked for), bound after the mounts below it (it would hide them), or
/// bound twice; and the overlay left out when no host mount is at the exec root.
#[test]
fn a_host_mount_at_the_exec_root_takes_the_overlays_place() {
    let mut job = spec();
    job.mounts = vec![
        host("/scratch", "/scratch/lease-1-2/job", false, false),
        host("/job/root/sdk", "/m/l/src/1", true, false),
        host("/job/root", "/m/l/cow/0", false, true),
    ];
    let config = config_of(&job);
    let mounts = config["mounts"].as_array().expect("mounts");
    let at_root: Vec<_> = mounts
        .iter()
        .filter(|m| m["destination"] == "/job/root")
        .collect();
    assert_eq!(
        at_root,
        [&json!({
            "destination": "/job/root",
            "type": "bind",
            "source": "/m/l/cow/0",
            "options": ["rbind", "rw", "nosuid", "nodev", "noexec"],
        })]
    );
    assert!(mounts.iter().all(|m| m["type"] != "overlay"), "{mounts:?}");
    let (root, _) = mount_at(&config, "/job/root").expect("the exec root");
    let (sdk, _) = mount_at(&config, "/job/root/sdk").expect("below it");
    let (scratch, _) = mount_at(&config, "/scratch").expect("scratch");
    let (tmp, _) = mount_at(&config, "/tmp").expect("tmp");
    assert!(root < tmp && tmp < scratch && scratch < sdk, "{mounts:?}");
    let plain = config_of(&spec());
    let (_, overlay) = mount_at(&plain, "/job/root").expect("the overlay");
    assert_eq!(overlay["type"], "overlay");
}
