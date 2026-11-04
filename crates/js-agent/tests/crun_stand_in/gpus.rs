//! A run that booked GPUs becoming a container: the bundle a staged run's lease holds, read as
//! `crun` would read it. The host's GPU nodes are a table (a test cannot make a character
//! device without root); what a real `crun` does with them is the `crun_runtime` target's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::crun::{CrunRuntime, CrunSettings, GpuPassthrough, GpuSettings, NO_GPUS};
use js_agent::lifecycle::{RunHandle, Runtime};
use js_contain::container::DeviceNode;
use serde_json::{Value, json};

use crate::node::{LONG, NoImages, Node};

/// The GPU indices the host reports.
const REPORTED: [u32; 4] = [0, 1, 2, 3];

/// A node of the host table, `/dev/<name>`, of device `major`:`minor`.
fn node_at(path: &Path, major: u32, minor: u32) -> DeviceNode {
    DeviceNode {
        path: path.to_string_lossy().into_owned(),
        major,
        minor,
        uid: 0,
        gid: 44,
        mode: 0o666,
    }
}

/// The nodes of a host with four NVIDIA GPUs: `nvidiactl` and `nvidia0` to `nvidia3`.
fn four_gpus(path: &Path) -> Result<DeviceNode, String> {
    let text = path.to_string_lossy();
    let gpu = text
        .strip_prefix("/dev/nvidia")
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| REPORTED.contains(n));
    match (text.as_ref(), gpu) {
        ("/dev/nvidiactl", _) => Ok(node_at(path, 195, 255)),
        (_, Some(n)) => Ok(node_at(path, 195, n)),
        (_, None) => Err(format!("{text}: No such file or directory")),
    }
}

/// The same host after GPU 2's node went away.
fn gpu_two_gone(path: &Path) -> Result<DeviceNode, String> {
    match path.to_str() {
        Some("/dev/nvidia2") => Err("/dev/nvidia2: No such file or directory".to_owned()),
        _ => four_gpus(path),
    }
}

/// The checked GPU flags `flags` on the four-GPU host.
fn passthrough(flags: &[&str]) -> GpuPassthrough {
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend_from_slice(flags);
    let cli = Cli::try_parse_from(args).expect("the flags parse");
    let checked = GpuPassthrough::from_cli(&cli, &REPORTED, four_gpus).expect("good flags");
    checked.expect("a device list")
}

/// The control node and one node per GPU.
const DEVICES: [&str; 4] = [
    "--gpu-device",
    "/dev/nvidiactl",
    "--gpu-device",
    "/dev/nvidia{index}",
];

/// A runtime on `node` that passes the GPUs `gpus` gives, reading nodes with `read`.
fn open_with_gpus(
    node: &Node,
    gpus: GpuPassthrough,
    read: js_agent::crun::ReadNode,
) -> CrunRuntime {
    let settings = CrunSettings {
        gpus: Some(GpuSettings {
            passthrough: gpus,
            read,
        }),
        ..node.settings("/root")
    };
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The bytes of the bundle of the staged container `id`.
fn bundle_bytes(node: &Node, id: &RunHandle) -> Vec<u8> {
    let path = node.lease_of(&id.0).join("bundle/config.json");
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The bundle of the staged container `id`.
fn bundle(node: &Node, id: &RunHandle) -> Value {
    serde_json::from_slice(&bundle_bytes(node, id)).expect("a config")
}

/// The run `run_id` booking the GPUs `indices`.
fn run(node: &Node, run_id: &str, indices: &[u32]) -> js_agent::lifecycle::RunSpec {
    let mut spec = node.spec(run_id);
    spec.gpu_indices = indices.to_vec();
    spec
}

/// The paths of the nodes a bundle gives its container.
fn device_paths(bundle: &Value) -> Vec<String> {
    bundle["linux"]["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|device| device["path"].as_str().expect("a path").to_owned())
        .collect()
}

/// Proves that, on a host with `--gpu-device /dev/nvidiactl --gpu-device /dev/nvidia{index}`
/// and four GPUs, a start that booked GPUs 1 and 3 is given `/dev/nvidiactl`, `/dev/nvidia1`
/// and `/dev/nvidia3`, each a character device with the numbers of its node, and no other
/// `/dev/nvidia*`; and that its device cgroup denies every device and allows those three and
/// nothing wider. Catches a job given every GPU, a device of an index it did not book, and an
/// allow rule wider than its list (a whole major number, or no deny first).
#[test]
fn a_gpu_job_gets_only_its_booked_devices() {
    let node = Node::new("gpu-booked");
    let mut runtime = open_with_gpus(&node, passthrough(&DEVICES), four_gpus);
    assert!(runtime.supports_gpus());
    let id = runtime
        .stage(&run(&node, "r1-hold", &[1, 3]))
        .expect("staged");
    let config = bundle(&node, &id);
    assert_eq!(
        device_paths(&config),
        ["/dev/nvidiactl", "/dev/nvidia1", "/dev/nvidia3"]
    );
    let numbers: Vec<(&str, u64, u64)> = config["linux"]["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .map(|d| {
            let n = |key: &str| d[key].as_u64().expect("a number");
            (d["type"].as_str().expect("a type"), n("major"), n("minor"))
        })
        .collect();
    assert_eq!(numbers, [("c", 195, 255), ("c", 195, 1), ("c", 195, 3)]);
    assert_eq!(
        config["linux"]["resources"]["devices"],
        json!([
            {"allow": false, "access": "rwm"},
            {"allow": true, "type": "c", "major": 195, "minor": 255, "access": "rw"},
            {"allow": true, "type": "c", "major": 195, "minor": 1, "access": "rw"},
            {"allow": true, "type": "c", "major": 195, "minor": 3, "access": "rw"},
        ])
    );
    runtime.remove(&id).expect("removed");
}

/// Proves a start that booked no GPU, on the same agent with devices, mounts and variables
/// listed, gets none of it: its bundle is byte for byte the bundle of an agent without the
/// flags. Catches GPU wiring that leaks into the other jobs of a GPU host.
#[test]
fn a_job_without_gpus_gets_no_gpu_configuration() {
    let node = Node::new("gpu-none");
    let plain = {
        let mut runtime = node.open();
        let id = runtime.stage(&node.spec("r1-hold")).expect("staged");
        let bytes = bundle_bytes(&node, &id);
        runtime.remove(&id).expect("removed");
        bytes
    };
    let driver = node.dir.join("driver");
    std::fs::create_dir_all(&driver).expect("a driver directory");
    let mount = format!("{}:/usr/local/driver", driver.display());
    let mut flags = DEVICES.to_vec();
    flags.extend(["--gpu-mount", &mount, "--gpu-env", "A=gpu"]);
    let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
    let id = runtime.stage(&node.spec("r1-hold")).expect("staged");
    let with_flags = bundle_bytes(&node, &id);
    assert!(
        plain == with_flags,
        "the bundle changed:\n{}\n---\n{}",
        String::from_utf8_lossy(&plain),
        String::from_utf8_lossy(&with_flags)
    );
    runtime.remove(&id).expect("removed");
}

/// What the runtime left on `node` for the run `run_id`: its lease directory, its cgroup and
/// its sidecar in the ledger.
fn left(node: &Node, run_id: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(&node.scratch)
        .expect("the store")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(run_id))
        .collect();
    let labels = node.scratch.join("oci-labels");
    found.extend(
        std::fs::read_dir(&labels)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path()),
    );
    let cgroups = node.cgroups.join("actions");
    found.extend(
        std::fs::read_dir(&cgroups)
            .expect("the actions cgroup")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.to_string_lossy().ends_with(run_id)),
    );
    found
}

/// Proves an agent without `--gpu-device` claims no GPU support and refuses a start that names
/// a GPU, with the words of the `gpu-unsupported` refusal, before it makes anything: no lease
/// directory, no cgroup, no sidecar. Catches a refusal that moved after the lease is made, and
/// a runtime that claims GPUs without a device list.
#[test]
fn without_gpu_device_flags_a_gpu_start_is_still_refused() {
    let node = Node::new("gpu-unsupported");
    let mut runtime = node.open();
    assert!(!runtime.supports_gpus());
    let why = runtime
        .stage(&run(&node, "r1-hold", &[0]))
        .expect_err("refused");
    assert_eq!(why.to_string(), NO_GPUS);
    assert_eq!(left(&node, "r1-hold"), Vec::<PathBuf>::new());
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves a GPU node that went away after the start check refuses the run when it starts,
/// naming the node, and makes nothing. Catches a missing node skipped in silence (the job would
/// run without the GPU it booked) and a refusal that leaves a lease behind.
#[test]
fn a_node_gone_since_the_start_refuses_the_run_and_makes_nothing() {
    let node = Node::new("gpu-gone");
    let mut runtime = open_with_gpus(&node, passthrough(&DEVICES), gpu_two_gone);
    let why = runtime
        .stage(&run(&node, "r1-hold", &[1, 2]))
        .expect_err("refused");
    assert_eq!(
        why.to_string(),
        "the GPU device /dev/nvidia2: No such file or directory"
    );
    assert_eq!(left(&node, "r1-hold"), Vec::<PathBuf>::new());
}

/// Proves the driver's mount of a GPU run is a bind of the directory the flag names, after the
/// run's own mounts, with exactly `rbind`, `ro`, `nosuid` and `nodev`; that its variables are
/// set, the one the image also sets taking the operator's value; and that the container keeps
/// its empty capability set and `noNewPrivileges`, so it cannot remount the driver writable.
/// Catches a writable driver mount, one without `nosuid` or `nodev`, a variable the image
/// hides, and GPU support that hands the job a capability.
#[test]
fn the_mounts_are_read_only_and_the_job_cannot_remount() {
    let node = Node::new("gpu-mounts");
    let driver = node.dir.join("driver");
    std::fs::create_dir_all(&driver).expect("a driver directory");
    let driver = std::fs::canonicalize(&driver).expect("canonical");
    let mount = format!("{}:/usr/local/driver", driver.display());
    let mut flags = DEVICES.to_vec();
    let env = [
        "--gpu-env",
        "A=gpu",
        "--gpu-env",
        "LD_LIBRARY_PATH=/usr/local/driver",
    ];
    flags.extend(["--gpu-mount", &mount]);
    flags.extend(env);
    // The image has the destination as a directory: the bind goes over it.
    let image_dir = image_root(&node).join("usr/local/driver");
    std::fs::create_dir_all(&image_dir).expect("a directory in the image");
    let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
    let id = runtime.stage(&run(&node, "r1-hold", &[0])).expect("staged");
    let config = bundle(&node, &id);
    let mounts = config["mounts"].as_array().expect("mounts");
    let at = |dest: &str| mounts.iter().position(|m| m["destination"] == dest);
    let driver_at = at("/usr/local/driver").expect("the driver's mount");
    assert!(at("/scratch").expect("the scratch mount") < driver_at);
    assert_eq!(
        mounts[driver_at],
        json!({
            "destination": "/usr/local/driver",
            "type": "bind",
            "source": driver,
            "options": ["rbind", "ro", "nosuid", "nodev"],
        })
    );
    let vars = config["process"]["env"].as_array().expect("env");
    assert!(vars.contains(&json!("A=gpu")), "{vars:?}");
    assert!(!vars.contains(&json!("A=1")), "{vars:?}");
    assert!(
        vars.contains(&json!("LD_LIBRARY_PATH=/usr/local/driver")),
        "{vars:?}"
    );
    let sets = config["process"]["capabilities"].as_object().expect("sets");
    assert_eq!(sets.len(), 5);
    for (name, set) in sets {
        assert_eq!(set, &json!([]), "{name}");
    }
    assert_eq!(config["process"]["noNewPrivileges"], true);
    runtime.remove(&id).expect("removed");
}

/// The unpacked root of the node's image.
fn image_root(node: &Node) -> PathBuf {
    js_contain::oci::image::published_root(&node.scratch, &node.digest)
}

/// Proves a driver mount whose destination is a file in the image (a library where the
/// operator named a directory) refuses the GPU run before anything is made, naming
/// `--gpu-mount` and the destination, even when the file is reached through a link in the
/// image; a run without GPUs of the same image still stages, and a GPU run whose destination
/// the image lacks stages too. Catches a start that `crun` fails
/// half way, with a lease, cgroup or sidecar left behind and an error that names no flag.
#[test]
fn a_mount_over_a_file_in_the_image_refuses_the_run_and_makes_nothing() {
    let node = Node::new("gpu-over-file");
    let driver = node.dir.join("driver");
    std::fs::create_dir_all(&driver).expect("a driver directory");
    let root = image_root(&node);
    std::fs::create_dir_all(root.join("usr/lib")).expect("a library directory");
    std::fs::write(root.join("usr/lib/libdriver.so.1"), b"").expect("a library");
    std::os::unix::fs::symlink("/usr/lib/libdriver.so.1", root.join("usr/lib/linked.so"))
        .expect("a link in the image");
    for target in ["/usr/lib/libdriver.so.1", "/usr/lib/linked.so"] {
        let mount = format!("{}:{target}", driver.display());
        let mut flags = DEVICES.to_vec();
        flags.extend(["--gpu-mount", &mount]);
        let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
        let why = runtime
            .stage(&run(&node, "r1-hold", &[0]))
            .expect_err("refused");
        assert_eq!(
            why.to_string(),
            format!(
                "--gpu-mount: the destination {target} is not a directory in the image, so the \
                 driver directory cannot be bound over it"
            )
        );
        assert_eq!(left(&node, "r1-hold"), Vec::<PathBuf>::new(), "{target}");
        let id = runtime
            .stage(&node.spec("r2-hold"))
            .expect("a run without GPUs");
        runtime.remove(&id).expect("removed");
    }
    // A destination the image does not have is made by `crun`: the run stages.
    let mount = format!("{}:/opt/driver", driver.display());
    let mut flags = DEVICES.to_vec();
    flags.extend(["--gpu-mount", &mount]);
    let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
    let id = runtime.stage(&run(&node, "r3-hold", &[0])).expect("staged");
    runtime.remove(&id).expect("removed");
}
