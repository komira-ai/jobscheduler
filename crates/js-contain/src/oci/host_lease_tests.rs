//! A host container's lease ([`Isolation::Host`]): who `crun` runs as for it, the bundle
//! written for it, and its start. The runtime is `/usr/bin/true`, so nothing runs.

use std::sync::Arc;

use super::tests::{config, scratch, start};
use super::*;
use crate::container::{ContainerSpec, Isolation};
use crate::subids::IdFiles;

/// A host container of the reference job in `dir`, whose exec root is `dir/run`.
fn host_in(dir: &Path) -> ContainerSpec {
    ContainerSpec {
        isolation: Isolation::Host,
        dir: dir.to_owned(),
        exec_root: dir.join("run").to_string_lossy().into_owned(),
        mounts: Vec::new(),
        ..crate::container::tests::spec()
    }
}

/// A backend that takes itself for a root daemon (`daemon` 0:0) dropping to a job user one
/// above this test's uid, in `dir`, and that job user's uid.
fn as_root(dir: &Path) -> (Arc<Oci>, u32) {
    let mut config = config(dir, true);
    let job = rustix::process::getuid().as_raw() + 1;
    let ids = IdFiles::in_dir(&dir.join("ids"));
    std::fs::write(&ids.passwd, format!("job:x:{job}:{job}::/:/bin/sh\n")).expect("passwd");
    std::fs::write(&ids.subuid, "job:300000:65536\n").expect("subuid");
    std::fs::write(&ids.subgid, "job:400000:65536\n").expect("subgid");
    config.job_user = Some((job, job));
    let oci = Oci::on_as(
        &config,
        Arc::new(super::tests::NoImages),
        &dir.join("s"),
        "/actions",
        "x86_64",
        (0, 0),
    )
    .expect("starts as root");
    (oci, job)
}

/// Proves a root daemon runs `crun` as the job user for a lease whose directory is not its
/// own (this test's, or none at all), and that a state ask and a kill do so: the drop to
/// the job user is tried, and refused, since this test is not root. Catches a
/// contained lease's kill or delete run as root.
#[tokio::test]
async fn a_root_daemon_runs_crun_as_the_job_user_for_a_lease_it_does_not_own() {
    let dir = scratch("host-lease-job-user");
    let (oci, job) = as_root(&dir);
    assert_eq!(oci.own_ids(), (0, 0));
    let lease = oci.lease_dirs("js-x-r1").dir;
    assert_eq!(oci.lease_user(&lease), job, "no directory");
    std::fs::create_dir_all(&lease).expect("lease");
    assert_eq!(oci.lease_user(&lease), job, "this test's directory");
    let spec = ContainerSpec {
        name: "js-x-r1".to_owned(),
        dir: lease,
        ..crate::container::tests::spec()
    };
    let refused = oci.container_pid(&spec).expect_err("the drop needs root");
    assert!(refused.contains("Operation not permitted"), "{refused}");
    let killed = oci
        .terminate("js-x-r1")
        .await
        .expect_err("the drop needs root");
    assert!(killed.contains("Operation not permitted"), "{killed}");
}

/// Proves a host container's bundle is refused in a lease directory that is not the
/// daemon's own, before anything is written: `crun` would run as the job user, who cannot
/// make it. Catches a host bundle in a directory the job user could have planted.
#[tokio::test]
async fn a_host_bundle_needs_a_lease_directory_of_the_daemons() {
    let dir = scratch("host-lease-not-own");
    let (oci, _) = as_root(&dir);
    let lease = dir.join("lease");
    let spec = host_in(&lease);
    let refused = oci.create(&spec).await.expect_err("not the daemon's");
    assert_eq!(
        refused,
        format!(
            "{}: a host container's lease directory is not the daemon's own",
            lease.display()
        )
    );
    assert!(!lease.join("bundle").exists());
}

/// Proves a daemon that owns the lease directory runs `crun` as itself for it, writes the
/// host container's `config.json` alone (no `etc` files: nothing is bound), with the
/// host's `/` as the root, and starts it with that `crun`; a kill and a delete by the
/// lease's name succeed with it. Catches a host bundle over the image's root, and one
/// whose start, kill or delete takes the job user's `crun`.
#[tokio::test]
async fn a_host_lease_of_the_daemons_gets_its_config_alone_and_the_daemons_crun() {
    let dir = scratch("host-lease-own");
    let oci = start(&config(&dir, true), &dir.join("s")).expect("started");
    let me = rustix::process::getuid().as_raw();
    let lease = oci.lease_dirs("js-x-r1").dir;
    let spec = ContainerSpec {
        name: "js-x-r1".to_owned(),
        ..host_in(&lease)
    };
    oci.create(&spec).await.expect("bundle");
    assert_eq!(oci.lease_user(&lease), me);
    let bytes = std::fs::read(lease.join("bundle/config.json")).expect("config.json");
    let config: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(config["root"]["path"], "/");
    assert_eq!(config["linux"]["namespaces"], serde_json::json!([]));
    assert!(!lease.join("etc").exists());
    let marker = std::fs::symlink_metadata(lease.join(HOST_LEASE_MARKER)).expect("the marker");
    assert!(marker.is_file());
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&marker), me);
    let mut running = oci.start(&spec, None).expect("started");
    running.wait().await;
    assert_eq!(running.run_end(), crun::RunEnd::Exited(0));
    oci.terminate("js-x-r1").await.expect("kill");
    oci.remove("js-x-r1").expect("delete");
}

/// Proves the owners the lease rule reads are of the right kind of file, never through a
/// link: a directory for the lease and a regular file for the marker; a file where the
/// lease should be, a directory where the marker should be, a link to either and a missing
/// path have no owner. Catches a marker a link points at, which could name a file the
/// daemon owns elsewhere.
#[test]
fn the_owners_of_a_lease_are_read_of_a_directory_and_a_regular_file_only() {
    let dir = scratch("host-lease-owners");
    let me = rustix::process::getuid().as_raw();
    let lease = dir.join("lease");
    std::fs::create_dir_all(lease.join("sub")).expect("a lease");
    std::fs::write(lease.join("file"), b"").expect("a file");
    std::os::unix::fs::symlink(lease.join("file"), lease.join("link")).expect("a link");
    std::os::unix::fs::symlink(&lease, dir.join("lease-link")).expect("a link");
    assert_eq!(owner_of(&lease, true), Some(me));
    assert_eq!(owner_of(&lease.join("file"), false), Some(me));
    for (path, dir) in [
        (lease.join("file"), true),
        (lease.join("sub"), false),
        (lease.join("link"), false),
        (dir.join("lease-link"), true),
        (lease.join("missing"), false),
    ] {
        assert_eq!(owner_of(&path, dir), None, "{}", path.display());
    }
}
