//! A run that booked GPUs becoming a container: the bundle a staged run's lease holds, read as
//! `crun` would read it. The host's GPU nodes are a table (a test cannot make a character
//! device without root); what a real `crun` does with them is the `crun_runtime` target's.
//! Nothing waits for a clock: runs are staged, or started with a program that exits at once,
//! and their end is waited for.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::cli_runtime::Settings;
use js_agent::crun::{
    CgroupSettings, CgroupSource, CrunRuntime, CrunSettings, DESTINATION_OFF_LIMITS,
    GpuPassthrough, GpuSettings, IdSources, NO_GPUS, NodeError, ReadNode, Seams, open_for_plan,
};
use js_agent::crun_host::CrunHost;
use js_agent::lifecycle::{RunHandle, Runtime, Status};
use js_contain::container::DeviceNode;
use js_contain::subids::IdFiles;
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

/// What a missing node reads as.
fn missing(path: &Path) -> NodeError {
    NodeError::Unreadable {
        path: path.to_owned(),
        error: io::Error::from_raw_os_error(2),
    }
}

/// The nodes of a host with four NVIDIA GPUs: `nvidiactl` and `nvidia0` to `nvidia3`.
fn four_gpus(path: &Path) -> Result<DeviceNode, NodeError> {
    let text = path.to_string_lossy();
    let gpu = text
        .strip_prefix("/dev/nvidia")
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| REPORTED.contains(n));
    match (text.as_ref(), gpu) {
        ("/dev/nvidiactl", _) => Ok(node_at(path, 195, 255)),
        (_, Some(n)) => Ok(node_at(path, 195, n)),
        (_, None) => Err(missing(path)),
    }
}

/// The same host after GPU 2's node went away.
fn gpu_two_gone(path: &Path) -> Result<DeviceNode, NodeError> {
    match path.to_str() {
        Some("/dev/nvidia2") => Err(missing(path)),
        _ => four_gpus(path),
    }
}

/// The same host after its nodes changed: GPU 3's node is a regular file, and every other
/// node has the major number 200, another owner and the mode 0600.
fn nodes_changed(path: &Path) -> Result<DeviceNode, NodeError> {
    if path == Path::new("/dev/nvidia3") {
        return Err(NodeError::NotCharacterDevice {
            path: path.to_owned(),
        });
    }
    four_gpus(path).map(|node| DeviceNode {
        major: 200,
        uid: 5,
        gid: 6,
        mode: 0o600,
        ..node
    })
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
fn open_with_gpus(node: &Node, gpus: GpuPassthrough, read: ReadNode) -> CrunRuntime {
    let settings = CrunSettings {
        gpus: Some(GpuSettings {
            passthrough: gpus,
            read,
        }),
        ..node.settings("/root")
    };
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The flags of the devices, a driver directory of `node` bound at `target`, and two
/// variables; the directory is made.
fn driver_flags(node: &Node, target: &str) -> (PathBuf, String) {
    let driver = node.dir.join("driver");
    std::fs::create_dir_all(&driver).expect("a driver directory");
    let driver = std::fs::canonicalize(&driver).expect("canonical");
    let mount = format!("{}:{target}", driver.display());
    (driver, mount)
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

/// The paths of the nodes a bundle gives its container (none when it has no device section).
fn device_paths(bundle: &Value) -> Vec<String> {
    bundle["linux"]["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|device| device["path"].as_str().expect("a path").to_owned())
        .collect()
}

/// The destinations of a bundle's mounts.
fn destinations(bundle: &Value) -> Vec<String> {
    bundle["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .map(|mount| mount["destination"].as_str().expect("a path").to_owned())
        .collect()
}

/// What the runtime left on `node` for the run `run_id`: its lease directory, its cgroup, its
/// sidecar in the ledger and its pin of the image.
fn left(node: &Node, run_id: &str) -> Vec<PathBuf> {
    let named = |dir: PathBuf| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(run_id))
            .collect()
    };
    let mut found = named(node.scratch.clone());
    found.extend(named(node.scratch.join("oci-labels")));
    found.extend(named(node.scratch.join("oci-image-pins/leases")));
    found.extend(named(node.cgroups.join("actions")));
    found
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
    let (_, mount) = driver_flags(&node, "/usr/local/driver");
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

/// Proves GPU runs that overlap stay apart and a run after them gets nothing of theirs: two
/// runs held at once with disjoint bookings (GPUs 0 and 2) are each given their own GPU's node
/// and not the other's; one of them is started and ends (its program exits 3, read as its
/// exit) and is removed, and the other's bundle is unchanged; then a run without GPUs of the
/// same image is given no device, no device rule, no driver mount and no GPU variable; and once
/// all are removed nothing of any is left (lease, cgroup, sidecar, pin). Catches one run's
/// nodes leaking into another's, an end that changes a run still held, and GPU wiring that
/// stays for the next run of the image.
#[test]
fn overlapping_gpu_runs_stay_apart_and_a_later_run_without_gpus_gets_nothing() {
    let node = Node::new("gpu-overlap");
    let (_, mount) = driver_flags(&node, "/usr/local/driver");
    let mut flags = DEVICES.to_vec();
    flags.extend(["--gpu-mount", &mount, "--gpu-env", "GPU_ONLY=1"]);
    let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
    let first = runtime.stage(&run(&node, "r1-exit", &[0])).expect("staged");
    let second = runtime.stage(&run(&node, "r2-hold", &[2])).expect("staged");
    assert_eq!(
        device_paths(&bundle(&node, &first)),
        ["/dev/nvidiactl", "/dev/nvidia0"]
    );
    let held = bundle_bytes(&node, &second);
    assert_eq!(
        device_paths(&bundle(&node, &second)),
        ["/dev/nvidiactl", "/dev/nvidia2"]
    );
    runtime.start_staged(&first).expect("started");
    runtime.wait_ended(&first);
    let ended = runtime.poll(&first).expect("polled");
    assert!(
        matches!(ended, Status::Exited(exit) if exit.code == 3),
        "{ended:?}"
    );
    runtime.remove(&first).expect("removed");
    assert_eq!(bundle_bytes(&node, &second), held);
    let later = runtime.stage(&node.spec("r3-hold")).expect("staged");
    let config = bundle(&node, &later);
    assert_eq!(device_paths(&config), Vec::<String>::new());
    assert!(config["linux"]["resources"].get("devices").is_none());
    assert!(!destinations(&config).contains(&"/usr/local/driver".to_owned()));
    let vars = config["process"]["env"].as_array().expect("env");
    assert!(!vars.contains(&json!("GPU_ONLY=1")), "{vars:?}");
    for id in [&second, &later] {
        runtime.remove(id).expect("removed");
    }
    for run_id in ["r1-exit", "r2-hold", "r3-hold"] {
        assert_eq!(left(&node, run_id), Vec::<PathBuf>::new(), "{run_id}");
    }
}

/// Proves an agent without `--gpu-device` claims no GPU support and refuses a start that names
/// a GPU, with the words of the `gpu-unsupported` refusal, before it makes anything: no lease
/// directory, no cgroup, no sidecar, no pin. Catches a refusal that moved after the lease is
/// made, and a runtime that claims GPUs without a device list.
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

/// Proves a run is given its nodes as they are when it starts, not as the start check read
/// them: nodes whose numbers, owner and mode changed are given with the new ones, and a node
/// that went away, or is a regular file now, refuses the run naming the node and makes
/// nothing. Catches nodes kept from the agent's start, a missing node skipped in silence (the
/// job would run without the GPU it booked), and a refusal that leaves a lease behind.
#[test]
fn a_run_is_given_its_nodes_as_they_are_when_it_starts() {
    let node = Node::new("gpu-changed");
    let mut runtime = open_with_gpus(&node, passthrough(&DEVICES), nodes_changed);
    let id = runtime.stage(&run(&node, "r1-hold", &[1])).expect("staged");
    let devices = bundle(&node, &id)["linux"]["devices"].clone();
    assert_eq!(
        devices,
        json!([
            {"path": "/dev/nvidiactl", "type": "c", "major": 200, "minor": 255,
             "fileMode": 0o600, "uid": 5, "gid": 6},
            {"path": "/dev/nvidia1", "type": "c", "major": 200, "minor": 1,
             "fileMode": 0o600, "uid": 5, "gid": 6},
        ])
    );
    runtime.remove(&id).expect("removed");
    let why = runtime
        .stage(&run(&node, "r2-hold", &[3]))
        .expect_err("refused");
    assert_eq!(
        why.to_string(),
        "the GPU device /dev/nvidia3 is not a character device"
    );
    assert_eq!(left(&node, "r2-hold"), Vec::<PathBuf>::new());
    drop(runtime);
    let mut runtime = open_with_gpus(&node, passthrough(&DEVICES), gpu_two_gone);
    let why = runtime
        .stage(&run(&node, "r3-hold", &[1, 2]))
        .expect_err("refused");
    assert_eq!(
        why.to_string(),
        "the GPU device /dev/nvidia2: No such file or directory (os error 2)"
    );
    assert_eq!(left(&node, "r3-hold"), Vec::<PathBuf>::new());
}

/// Proves the driver's mount of a GPU run is a bind of the directory the flag names, after the
/// run's own mounts, with exactly `bind` (not recursive: no mount below the driver directory
/// on the host, writable or not, reaches the job), `ro`, `nosuid` and `nodev`; that its
/// variables are set, the one the image also sets taking the operator's value; and that the
/// container keeps its empty capability set and `noNewPrivileges`, so it cannot remount the
/// driver writable. Catches a writable or recursive driver mount, one without `nosuid` or
/// `nodev`, a variable the image hides, and GPU support that hands the job a capability.
#[test]
fn the_mounts_are_read_only_and_the_job_cannot_remount() {
    let node = Node::new("gpu-mounts");
    let (driver, mount) = driver_flags(&node, "/usr/local/driver");
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
            "options": ["bind", "ro", "nosuid", "nodev"],
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

/// Proves no `--gpu-mount` destination can be one the runtime mounts itself: every mount of
/// a GPU run's bundle but the driver's is at or below a place a driver mount may not go. A
/// mount the runtime adds later at a place not on that list fails this, so the list cannot
/// drift from the bundle. Catches a driver directory bound over the job's overlay, home,
/// tmpfs (the run's files are in `/run`) or name files, where the image check would look at
/// the wrong tree.
#[test]
fn every_mount_the_runtime_makes_is_off_limits_to_a_driver_mount() {
    let node = Node::new("gpu-off-limits");
    let mut runtime = open_with_gpus(&node, passthrough(&DEVICES), four_gpus);
    let id = runtime.stage(&run(&node, "r1-hold", &[0])).expect("staged");
    let below = |path: &str, tree: &str| {
        path.strip_prefix(tree)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    for destination in destinations(&bundle(&node, &id)) {
        assert!(
            DESTINATION_OFF_LIMITS
                .iter()
                .any(|tree| below(&destination, tree)),
            "{destination} is not off limits to a driver mount"
        );
    }
    runtime.remove(&id).expect("removed");
}

/// The unpacked root of the node's image.
fn image_root(node: &Node) -> PathBuf {
    js_contain::oci::image::published_root(&node.scratch, &node.digest)
}

/// Proves a driver mount whose destination the image has as a file (a library where the
/// operator named a directory) or as a link (here to the file, inside the image) refuses the
/// GPU run before anything is made, naming `--gpu-mount`, the destination and the part; a run
/// without GPUs of the same image still stages, and a GPU run whose destination the image
/// lacks stages too. Catches a start that `crun` fails half way, with a lease, cgroup, sidecar
/// or pin left behind and an error that names no flag, and a bind a link in the image redirects.
#[test]
fn a_mount_over_a_file_in_the_image_refuses_the_run_and_makes_nothing() {
    let node = Node::new("gpu-over-file");
    let (driver, _) = driver_flags(&node, "/opt/driver");
    let root = image_root(&node);
    std::fs::create_dir_all(root.join("usr/lib")).expect("a library directory");
    std::fs::write(root.join("usr/lib/libdriver.so.1"), b"").expect("a library");
    std::os::unix::fs::symlink("/usr/lib/libdriver.so.1", root.join("usr/lib/linked.so"))
        .expect("a link in the image");
    for (target, why) in [
        (
            "/usr/lib/libdriver.so.1",
            "the image has something other than a directory at /usr/lib/libdriver.so.1",
        ),
        (
            "/usr/lib/linked.so",
            "the image has a link at /usr/lib/linked.so, which a driver mount does not follow",
        ),
    ] {
        let mount = format!("{}:{target}", driver.display());
        let mut flags = DEVICES.to_vec();
        flags.extend(["--gpu-mount", &mount]);
        let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
        let refused = runtime
            .stage(&run(&node, "r1-hold", &[0]))
            .expect_err("refused");
        assert_eq!(
            refused.to_string(),
            format!("--gpu-mount: the destination {target}: {why}")
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

/// Proves a driver directory that went away after the start check, became a file, or sits below
/// a directory the agent can no longer search, refuses
/// the GPU run when it starts, naming `--gpu-mount` and the directory, and makes nothing (no
/// lease, cgroup, sidecar or pin); and that once the directory is back the same runtime gives
/// the next run its GPU and the driver again. Catches a run started without its driver (or
/// with a file bound where a directory was checked), a refusal that leaves a lease behind,
/// and a runtime that stays broken after the directory is restored.
#[test]
fn a_driver_directory_gone_since_the_start_refuses_the_run_and_makes_nothing() {
    let node = Node::new("gpu-driver-gone");
    let (driver, mount) = driver_flags(&node, "/usr/local/driver");
    let mut flags = DEVICES.to_vec();
    flags.extend(["--gpu-mount", &mount]);
    let mut runtime = open_with_gpus(&node, passthrough(&flags), four_gpus);
    std::fs::remove_dir(&driver).expect("the driver goes");
    let gone = runtime
        .stage(&run(&node, "r1-hold", &[0]))
        .expect_err("refused");
    let shown = driver.display();
    assert_eq!(
        gone.to_string(),
        format!(
            "--gpu-mount: the source {shown} is not a directory now: No such file or directory \
             (os error 2)"
        )
    );
    assert_eq!(left(&node, "r1-hold"), Vec::<PathBuf>::new());
    std::fs::write(&driver, b"").expect("a file in its place");
    let file = runtime
        .stage(&run(&node, "r2-hold", &[0]))
        .expect_err("refused");
    assert_eq!(
        file.to_string(),
        format!("--gpu-mount: the source {shown} is not a directory now: something else is there")
    );
    assert_eq!(left(&node, "r2-hold"), Vec::<PathBuf>::new());
    std::fs::remove_file(&driver).expect("the file goes");
    std::fs::create_dir(&driver).expect("the driver is back");
    let id = runtime.stage(&run(&node, "r3-hold", &[0])).expect("staged");
    let config = bundle(&node, &id);
    assert_eq!(device_paths(&config), ["/dev/nvidiactl", "/dev/nvidia0"]);
    assert!(destinations(&config).contains(&"/usr/local/driver".to_owned()));
    runtime.remove(&id).expect("removed");
    drop(runtime);
    // A directory above the driver that the agent may not search (the test is not root).
    let above = node.dir.join("driver-above");
    std::fs::create_dir_all(above.join("driver")).expect("a driver below a directory");
    let hidden = std::fs::canonicalize(above.join("driver")).expect("canonical");
    let mut flags = DEVICES.to_vec();
    let hidden_mount = format!("{}:/usr/local/hidden", hidden.display());
    flags.extend(["--gpu-mount", &hidden_mount]);
    let mut locked = open_with_gpus(&node, passthrough(&flags), four_gpus);
    let mode = |bits| std::os::unix::fs::PermissionsExt::from_mode(bits);
    std::fs::set_permissions(&above, mode(0)).expect("lock it");
    let denied = locked.stage(&run(&node, "r4-hold", &[0]));
    std::fs::set_permissions(&above, mode(0o755)).expect("unlock it");
    assert_eq!(
        denied.expect_err("refused").to_string(),
        format!(
            "--gpu-mount: the source {} is not a directory now: Permission denied (os error 13)",
            hidden.display()
        )
    );
    assert_eq!(left(&node, "r4-hold"), Vec::<PathBuf>::new());
    let id = locked.stage(&run(&node, "r5-hold", &[0])).expect("staged");
    locked.remove(&id).expect("removed");
    drop(locked);
}

/// Proves the runtime the agent opens from its settings (`open_for_plan`, what the worker
/// calls) carries the device list of the crun settings and reads the nodes from the host when
/// a run starts: it supports GPUs, and a GPU run whose node the host does not have is refused
/// naming it, with the host's own reason. Without a device list it supports none. Catches a device list lost between the flags and the runtime, and a runtime that
/// reads nodes from somewhere other than the host.
#[test]
fn the_runtime_opened_from_the_settings_passes_the_gpus_and_reads_the_host() {
    let node = Node::new("gpu-plan");
    let scratch_root = node.dir.join("runs-root");
    std::fs::create_dir_all(&scratch_root).expect("a scratch root");
    let seams = Seams {
        cgroups: CgroupSource::Fixed(CgroupSettings {
            mount: node.cgroups.clone(),
            root: "/root".to_owned(),
            actions: "/actions".to_owned(),
        }),
        hooks: Node::hooks(LONG, LONG),
        host: CrunHost::system,
        ids: IdSources {
            files: IdFiles::in_dir(&node.dir.join("ids")),
            tool_path: node.dir.join("tools").into_os_string(),
            check_search: false,
        },
    };
    let settings = |gpus: Option<GpuPassthrough>| {
        let crun = node.dir.join("crun");
        Settings::for_crun(crun, "jsjob", (node.uid, node.gid), Vec::new())
            .placed_beside(&scratch_root, None)
            .with_gpus(gpus)
    };
    let plain = settings(None);
    let plan = plain.crun().expect("crun settings");
    let (runtime, _, _) = open_for_plan(plan, "node-1", &seams).expect("opens");
    assert!(!runtime.supports_gpus());
    drop(runtime);
    let cli = Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "p",
        "--gpu-device",
        NO_SUCH_NODE,
    ])
    .expect("the flags parse");
    let listed = GpuPassthrough::from_cli(&cli, &REPORTED, any_node).expect("good flags");
    let gpus = settings(listed);
    let plan = gpus.crun().expect("crun settings");
    let (mut runtime, _, _) = open_for_plan(plan, "node-1", &seams).expect("opens");
    assert!(runtime.supports_gpus());
    let refused = runtime
        .stage(&run(&node, "r1-hold", &[0]))
        .expect_err("no such node on the host");
    assert_eq!(
        refused.to_string(),
        format!("the GPU device {NO_SUCH_NODE}: No such file or directory (os error 2)")
    );
}

/// A node no host has.
const NO_SUCH_NODE: &str = "/dev/js-agent-test-no-such-node";

/// A reader that finds a character device at every path: what the start check of a host
/// that had the node read.
fn any_node(path: &Path) -> Result<DeviceNode, NodeError> {
    Ok(node_at(path, 1, 1))
}
