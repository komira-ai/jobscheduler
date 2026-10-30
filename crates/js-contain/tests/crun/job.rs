//! A job over the published image root, with the real runtime.

use std::sync::Arc;

use js_contain::cgroup::LeaseCgroup;
use js_contain::container::{CONTAINER_OWNER, ContainerLimits, ContainerSpec, Dirs, Ended};
use js_contain::oci::image::ROOTFS;
use js_types::Resources;

use crate::cell::{Cell, MOUNT};
use crate::source::{Registry, busybox};

/// Runs `argv` (a program of the image, by path) as lease `name` over the image and says
/// how it ended and what it wrote to stderr.
async fn run(cell: &Cell, name: &str, argv: &[&str]) -> (Ended, String) {
    let image = busybox();
    let lease = cell.scratch.join(name);
    let (root, upper, work) = (lease.join("root"), lease.join("upper"), lease.join("work"));
    for dir in [&root, &upper, &work] {
        std::fs::create_dir_all(dir).expect("lease directory");
    }
    let dirs = Dirs {
        lease: &lease,
        root: &root,
        upper: &upper,
        work: &work,
        helper: None,
    };
    cell.oci.give(CONTAINER_OWNER, &dirs).await.expect("given");
    let cgroup = LeaseCgroup::at(std::path::Path::new(MOUNT), &cell.cgroup, name);
    let booking = Resources::new(1000, 256 << 20, 0, None).expect("a booking");
    cgroup.create(&booking, None).expect("the lease cgroup");
    let spec = ContainerSpec {
        name: name.to_owned(),
        image: image.reference.clone(),
        image_digest: image.digest.clone(),
        dir: lease.clone(),
        cgroup_parent: cgroup.name().to_owned(),
        exec_root: "/job/root".to_owned(),
        input_root: root,
        upper,
        overlay_work: work,
        working_directory: String::new(),
        env: Vec::new(),
        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        limits: ContainerLimits::DEFAULT,
        shared: Vec::new(),
    };
    cell.oci.create(&spec).await.expect("bundle");
    let stdout = std::fs::File::create(lease.join("stdout")).expect("stdout");
    let stderr = std::fs::File::create(lease.join("stderr")).expect("stderr");
    let mut running = cell.oci.start(&spec, stdout, stderr).expect("started");
    running.wait().await;
    let ended = cell.oci.ended(&spec, &running).await.expect("ended");
    let said = std::fs::read_to_string(lease.join("stderr")).unwrap_or_default();
    let _ = cell.oci.remove(name);
    let _ = cgroup.remove();
    (ended, said)
}

/// Catches the shared image root being writable from a job: the root is the one
/// directory every job of the image shares (the overlay's upper layer is where a job
/// writes), so creating a file or a directory in it, or changing an existing file, is
/// refused with "Read-only file system", and the stored root has none of them after.
#[tokio::test]
#[ignore = "real crun and the network: run through cargo xtask crun-tests"]
async fn a_job_cannot_write_the_shared_image_root() {
    let cell = Cell::new("erofs", Arc::new(Registry::new()));
    let image = busybox();
    cell.oci.check_pinned(&image).await.expect("the image");
    let attempts: [&[&str]; 3] = [
        &["/bin/touch", "/new-file"],
        &["/bin/mkdir", "/new-dir"],
        &["/bin/touch", "/bin/busybox"],
    ];
    for (i, argv) in attempts.iter().enumerate() {
        let (ended, said) = run(&cell, &format!("lease-{i}"), argv).await;
        assert!(
            matches!(ended, Ended::Exited(code) if code != 0),
            "{argv:?}: {ended:?} {said}"
        );
        assert!(said.contains("Read-only file system"), "{argv:?}: {said}");
    }
    let root = cell.image_dir(&image.digest).join(ROOTFS);
    for name in ["new-file", "new-dir"] {
        assert!(
            std::fs::symlink_metadata(root.join(name)).is_err(),
            "{name}"
        );
    }
}
