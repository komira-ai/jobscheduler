//! A GPU job over a real `crun` on a host with at least two NVIDIA GPUs: the job sees the node
//! of the GPU it booked and not the other's.
//!
//! The test needs what every test of this target needs (root in a `Delegate=yes` unit, the job
//! user `JS_JOB_USER`, the image `JS_TEST_IMAGE`) and, beside it, the NVIDIA driver's nodes
//! `/dev/nvidiactl`, `/dev/nvidia0` and `/dev/nvidia1`; without them it fails, naming the node.
//! No hosted runner has a GPU, so the `js-agent-crun-runtime` workflow skips it by name, and it
//! is run by hand on a GPU host: the target's binary, as that workflow runs it, with
//! `--ignored a_crun_job_sees_only_its_booked_gpu`.

use std::path::Path;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::crun::{GpuPassthrough, GpuSettings, read_node};
use js_agent::lifecycle::{Body, Exit, Runtime, Status};

use crate::runtime_suite::{Program, Subject, spec_of};
use crate::subject;

/// The NVIDIA nodes the test needs: the control node and two GPUs'.
const NODES: [&str; 3] = ["/dev/nvidiactl", "/dev/nvidia0", "/dev/nvidia1"];

/// Proves, over a real `crun`, that a job that booked GPU 1 of a host with two sees
/// `/dev/nvidiactl` and `/dev/nvidia1` in its `/dev` and not `/dev/nvidia0`, with the variable
/// `--gpu-env` names set, and exits 0. Catches a bundle `crun` gives every GPU, one whose
/// booked node does not appear (a device list `crun` does not honour), and a device rule that
/// keeps the job from starting.
#[test]
#[ignore = "real crun on a host with two NVIDIA GPUs: run by hand (see the module)"]
fn a_crun_job_sees_only_its_booked_gpu() {
    for node in NODES {
        read_node(Path::new(node))
            .unwrap_or_else(|e| panic!("this test needs a host with two NVIDIA GPUs: {e}"));
    }
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
        "--gpu-env",
        "GPU_TEST=booked",
    ])
    .expect("the flags parse");
    let passthrough = GpuPassthrough::from_cli(&cli, &[0, 1], read_node)
        .expect("the host has the nodes")
        .expect("a device list");
    let mut s = subject::crun_subject("a_crun_job_sees_only_its_booked_gpu");
    s.prepared();
    let gpus = GpuSettings {
        passthrough,
        read: read_node,
    };
    let mut runtime = s.open_with_gpus("crun-suite-gpu", gpus);
    let mut spec = spec_of(&mut s, "r1", Program::Exit(0));
    spec.gpu_indices = vec![1];
    let script = "ls -1 /dev; echo GPU_TEST=$GPU_TEST";
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
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
    let listed: Vec<&str> = tail.stdout.lines().collect();
    for want in ["nvidiactl", "nvidia1", "GPU_TEST=booked"] {
        assert!(listed.contains(&want), "{want} not in {listed:?}");
    }
    assert!(!listed.contains(&"nvidia0"), "the other GPU: {listed:?}");
    runtime.remove(&id).expect("remove");
}
