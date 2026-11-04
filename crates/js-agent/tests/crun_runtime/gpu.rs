//! A GPU job over a real `crun` on a host with at least two NVIDIA GPUs: the job sees the node
//! of the GPU it booked and not the other's, can open it, and reads its driver directory but
//! cannot write it.
//!
//! The test needs what every test of this target needs (root in a `Delegate=yes` unit, the job
//! user `JS_JOB_USER`, the image `JS_TEST_IMAGE`) and, beside it, the NVIDIA driver's nodes
//! `/dev/nvidiactl`, `/dev/nvidia0` and `/dev/nvidia1`; without them it fails, naming the node.
//! No hosted runner has a GPU, so the `js-agent-crun-runtime` workflow skips it by name, and it
//! is run by hand on a GPU host: the target's binary, as that workflow runs it, with
//! `--ignored a_crun_job_sees_only_its_booked_gpu`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::crun::{GpuPassthrough, GpuSettings, read_node};
use js_agent::lifecycle::{Body, Exit, Runtime, Status};

use crate::runtime_suite::{Program, Subject, spec_of};
use crate::subject;

/// The NVIDIA nodes the test needs: the control node and two GPUs'.
const NODES: [&str; 3] = ["/dev/nvidiactl", "/dev/nvidia0", "/dev/nvidia1"];

/// What the job prints: its `/dev`, then one line for each check.
const SCRIPT: &str = "ls -1 /dev; echo GPU_TEST=$GPU_TEST; \
    echo \"node $(stat -c '%F %t %T' /dev/nvidia1)\"; echo \"mode $(stat -c '%a' /dev/nvidia1)\"; \
    if [ -e /usr/local/gpu-test/sub/inner ]; then echo sub-seen; else echo sub-hidden; fi; \
    if : < /dev/nvidia1; then echo opened; else echo not-opened; fi; \
    echo \"driver $(cat /usr/local/gpu-test/marker)\"; \
    if touch /usr/local/gpu-test/written 2>/dev/null; then echo wrote; else echo read-only; fi; \
    if mount -o remount,rw /usr/local/gpu-test 2>/dev/null; then echo remounted; else echo kept; fi";

/// Proves, over a real `crun` and as the job user, that a job that booked GPU 1 of a host with
/// two sees `/dev/nvidiactl` and `/dev/nvidia1` in its `/dev` and not `/dev/nvidia0`; that
/// `/dev/nvidia1` there is a character device with the host node's major and minor numbers
/// and mode, and that the job can open it; that a writable mount below the driver directory
/// on the host does not reach the job (the bind is not recursive); that the variable `--gpu-env` names is set; and that the
/// driver directory is there to read, but cannot be written or remounted read-write. Catches
/// a bundle `crun` gives every GPU, a booked node that does not appear or that the job user
/// cannot open, a node of another type, device or mode, a device rule that keeps the job from
/// starting, a writable driver mount (the directory itself is writable by the job's id, so a
/// failed write is the bind's), a driver submount that leaks into the job, and a later job
/// without GPUs of the same image that sees the driver or a GPU node.
#[test]
#[ignore = "real crun on a host with two NVIDIA GPUs: run by hand (see the module)"]
fn a_crun_job_sees_only_its_booked_gpu() {
    for node in NODES {
        read_node(Path::new(node))
            .unwrap_or_else(|e| panic!("this test needs a host with two NVIDIA GPUs: {e}"));
    }
    let host_node = read_node(Path::new("/dev/nvidia1")).expect("GPU 1's node");
    let mut s = subject::crun_subject("a_crun_job_sees_only_its_booked_gpu");
    let driver = s.searchable_dir("gpu-test-driver");
    std::fs::write(driver.join("marker"), "from-the-host").expect("a file in the driver");
    // Writable by everyone, the job's host id among them: a write that fails in the job is
    // the read-only bind's doing, not the directory's mode.
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o777))
        .expect("make the driver writable");
    let mode = std::fs::metadata(&driver)
        .expect("the driver")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o777,
        "the job's id could write the driver directory"
    );
    // A writable mount below the driver directory: the bind is not recursive, so the job must
    // see the empty directory under it and nothing of the mount.
    let sub = driver.join("sub");
    std::fs::create_dir(&sub).expect("a mount point");
    let mounted = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&sub)
        .status()
        .expect("run mount");
    assert!(mounted.success(), "mount a tmpfs below the driver");
    std::fs::write(sub.join("inner"), "below").expect("a file in the submount");
    let mount = format!("{}:/usr/local/gpu-test", driver.display());
    let cli = Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "p",
        "--gpu-device",
        "/dev/nvidiactl",
        "--gpu-device",
        "/dev/nvidia{index}",
        "--gpu-mount",
        &mount,
        "--gpu-env",
        "GPU_TEST=booked",
    ])
    .expect("the flags parse");
    let passthrough = GpuPassthrough::from_cli(&cli, &[0, 1], read_node)
        .expect("the host has the nodes")
        .expect("a device list");
    s.prepared();
    let in_image = s.image_root().join("usr/local/gpu-test");
    assert!(
        !in_image.exists(),
        "the image has no {}",
        in_image.display()
    );
    let gpus = GpuSettings {
        passthrough,
        read: read_node,
    };
    let mut runtime = s.open_with_gpus("crun-suite-gpu", gpus);
    let mut spec = spec_of(&mut s, "r1", Program::Exit(0));
    spec.gpu_indices = vec![1];
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), SCRIPT.to_owned()],
    };
    let id = runtime.create_and_start(&spec).expect("create and start");
    runtime.wait_ended(&id);
    let ended = runtime.poll(&id).expect("poll");
    let tail = runtime.log_tail(&id).expect("a tail").expect("the tail");
    assert_eq!(
        ended,
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        }),
        "{tail:?}"
    );
    let unmounted = std::process::Command::new("umount")
        .arg(&sub)
        .status()
        .expect("run umount");
    assert!(unmounted.success(), "unmount the tmpfs below the driver");
    let listed: Vec<&str> = tail.stdout.lines().collect();
    let mode_line = format!("mode {:o}", host_node.mode & 0o777);
    let node_line = format!(
        "node character special file {:x} {:x}",
        host_node.major, host_node.minor
    );
    for want in [
        "nvidiactl",
        "nvidia1",
        "GPU_TEST=booked",
        node_line.as_str(),
        mode_line.as_str(),
        "sub-hidden",
        "opened",
        "driver from-the-host",
        "read-only",
        "kept",
    ] {
        assert!(listed.contains(&want), "{want} not in {listed:?}");
    }
    assert!(!listed.contains(&"nvidia0"), "the other GPU: {listed:?}");
    runtime.remove(&id).expect("remove");

    // A job without GPUs of the same image, after `crun` made the driver's destination in the
    // image's root: it sees at most an empty directory there, and runs.
    let mut plain = spec_of(&mut s, "r2", Program::Exit(0));
    plain.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), AFTER.to_owned()],
    };
    let id = runtime.create_and_start(&plain).expect("create and start");
    runtime.wait_ended(&id);
    let ended = runtime.poll(&id).expect("poll");
    let tail = runtime.log_tail(&id).expect("a tail").expect("the tail");
    assert_eq!(
        ended,
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        }),
        "{tail:?}"
    );
    let listed: Vec<&str> = tail.stdout.lines().collect();
    assert!(listed.contains(&"ran"), "{listed:?}");
    assert!(
        listed.contains(&"absent") || listed.contains(&"left 0"),
        "a job without GPUs sees the driver's files: {listed:?}"
    );
    assert!(!listed.contains(&"nvidiactl"), "a GPU node: {listed:?}");
    runtime.remove(&id).expect("remove");
}

/// What the job without GPUs prints: what is at the driver's destination, its `/dev`, and that
/// it ran.
const AFTER: &str = "if [ -e /usr/local/gpu-test ]; then \
    echo \"left $(ls -A /usr/local/gpu-test | wc -l)\"; else echo absent; fi; ls -1 /dev; echo ran";
