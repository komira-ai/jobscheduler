//! The GPU flags of the crun runtime and what a run that booked GPUs is given. The host's
//! nodes are a table here ([`host_nodes`]): a test cannot make a character device without
//! root. The host's own reader is tested on `/dev/null`, which every Linux host has.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use js_contain::container::{ContainerLimits, ContainerSpec, DeviceNode, HostMount};
use js_testkit::dir::TestDir;

use super::{GpuPassthrough, GpuSettings, NO_GPUS, read_node, wiring};
use crate::Cli;
use crate::cli_runtime::Settings;

/// The indices of the GPUs of the host the tests check against.
const REPORTED: [u32; 4] = [0, 1, 2, 3];

/// The nodes of a host with four NVIDIA GPUs: `nvidiactl`, `nvidia-uvm` and `nvidia0` to
/// `nvidia3`, each a character device owned by root and group 44; any other path is missing.
fn host_nodes(path: &Path) -> Result<DeviceNode, String> {
    let text = path.to_string_lossy();
    let gpu = text
        .strip_prefix("/dev/nvidia")
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| REPORTED.contains(n));
    let (major, minor) = match (text.as_ref(), gpu) {
        ("/dev/nvidiactl", _) => (195, 255),
        ("/dev/nvidia-uvm", _) => (511, 0),
        (_, Some(n)) => (195, n),
        (_, None) => return Err(format!("{text}: No such file or directory")),
    };
    Ok(DeviceNode {
        path: text.into_owned(),
        major,
        minor,
        uid: 0,
        gid: 44,
        mode: 0o666,
    })
}

/// The agent's command line with `flags`.
fn cli(flags: &[&str]) -> Cli {
    let mut args = vec!["js-agent", "--server", "http://127.0.0.1:1", "--pool", "p"];
    args.extend_from_slice(flags);
    Cli::try_parse_from(args).expect("the flags parse")
}

/// `--gpu-device` for each of `devices`, and `more` after them.
fn with_devices(devices: &[&str], more: &[&str]) -> Cli {
    let mut flags = Vec::new();
    for device in devices {
        flags.extend(["--gpu-device", device]);
    }
    flags.extend_from_slice(more);
    cli(&flags)
}

/// What the flags `cli` give on the four-GPU host.
fn checked(cli: &Cli) -> Result<Option<GpuPassthrough>, String> {
    GpuPassthrough::from_cli(cli, &REPORTED, host_nodes)
}

/// The refusal of the flags `cli` on the four-GPU host.
fn refusal(cli: &Cli) -> String {
    checked(cli).expect_err("refused")
}

/// The usual device list: the control node and one node per GPU.
const NVIDIA: [&str; 2] = ["/dev/nvidiactl", "/dev/nvidia{index}"];

/// Proves an agent without GPU flags passes no GPU (the runtime keeps refusing GPU starts),
/// and that `--gpu-mount` and `--gpu-env` without a device list are refused naming the flag
/// and what it needs. Catches an agent that claims GPU support without a device list, and
/// mounts or variables that would silently go nowhere.
#[test]
fn without_a_device_list_no_gpu_is_passed_and_the_other_flags_are_refused() {
    assert_eq!(checked(&cli(&[])), Ok(None));
    let mount = refusal(&cli(&["--gpu-mount", "/opt/driver:/usr/local/driver"]));
    assert_eq!(mount, "--gpu-mount: needs --gpu-device");
    let env = refusal(&cli(&["--gpu-env", "PATH=/x"]));
    assert_eq!(env, "--gpu-env: needs --gpu-device");
}

/// Proves a device list on a host that offers no GPU is refused: there is nothing to give a
/// job, and no per-GPU node could be checked. Catches a template checked against nothing.
#[test]
fn a_device_list_needs_a_host_with_gpus() {
    let why = GpuPassthrough::from_cli(&with_devices(&NVIDIA, &[]), &[], host_nodes);
    assert_eq!(
        why,
        Err("--gpu-device: the host offers no GPU to give a job".to_owned())
    );
}

/// Proves each malformed `--gpu-device` is refused naming the flag and the path: relative, not
/// below `/dev`, `/dev` itself, with an empty, `.` or `..` part, `{index}` twice, a brace that
/// is not `{index}`, and the same path twice. Catches a node made outside the container's own
/// `/dev`, a path that climbs out of it, and a typo of `{index}` taken for a common node.
#[test]
fn a_malformed_device_path_is_refused_naming_it() {
    let below = "is not an absolute path below /dev without empty, `.` or `..` parts";
    for (path, why) in [
        ("nvidia0", below),
        ("dev/nvidia0", below),
        ("/tmp/nvidia0", below),
        ("/dev", below),
        ("/dev/", below),
        ("/dev//nvidia0", below),
        ("/dev/./nvidia0", below),
        ("/dev/../dev/nvidia0", below),
        ("/dev/nvidia{index}/{index}", "names {index} more than once"),
        (
            "/dev/nvidia{idx}",
            "has a brace that is not part of {index}",
        ),
        (
            "/dev/nvidia{index}}",
            "has a brace that is not part of {index}",
        ),
        (
            "/dev/{nvidia{index}",
            "has a brace that is not part of {index}",
        ),
    ] {
        let refused = refusal(&with_devices(&[path], &[]));
        assert_eq!(refused, format!("--gpu-device: {path} {why}"), "{path}");
    }
    let twice = refusal(&with_devices(&["/dev/nvidiactl", "/dev/nvidiactl"], &[]));
    assert_eq!(twice, "--gpu-device: /dev/nvidiactl is given twice");
}

/// Proves a node that is not on the host is refused at start naming the flag and the node: a
/// common one, and a per-GPU one whose node is missing for one GPU the host reports. Catches
/// a missing node skipped in silence (the job would start without its GPU), and a template
/// checked for the first GPU only.
#[test]
fn a_missing_node_is_refused_at_start_naming_it() {
    let common = refusal(&with_devices(&["/dev/nvidia-modeset"], &[]));
    assert_eq!(
        common,
        "--gpu-device: /dev/nvidia-modeset: No such file or directory"
    );
    let reported = [0, 1, 7];
    let gpu = GpuPassthrough::from_cli(&with_devices(&NVIDIA, &[]), &reported, host_nodes);
    assert_eq!(
        gpu,
        Err("--gpu-device: /dev/nvidia7: No such file or directory".to_owned())
    );
}

/// Proves the host's reader gives a character device's numbers, owner and mode at the path
/// it was asked for, and refuses a regular file, a directory, a link to a character device
/// (it is not followed) and a path that is not there, naming the path. Catches a reader that
/// accepts any file, or follows a link a job's writer could have placed.
#[test]
fn the_hosts_reader_takes_character_devices_only() {
    let null = read_node(Path::new("/dev/null")).expect("/dev/null is a character device");
    assert_eq!(
        (null.path.as_str(), null.major, null.minor),
        ("/dev/null", 1, 3)
    );
    let meta = std::fs::metadata("/dev/null").expect("/dev/null");
    assert_eq!(
        (null.uid, null.gid, null.mode),
        (meta.uid(), meta.gid(), meta.mode() & 0o7777)
    );
    let dir = TestDir::under(std::env::temp_dir(), "gpu-read-node");
    let file = dir.join("file");
    std::fs::write(&file, b"").expect("a file");
    let link = dir.join("link");
    std::os::unix::fs::symlink("/dev/null", &link).expect("a link");
    let itself = dir.to_path_buf();
    for path in [&file, &itself, &link] {
        let why = read_node(path).expect_err("not a character device");
        assert_eq!(why, format!("{} is not a character device", path.display()));
    }
    let gone = read_node(&dir.join("gone")).expect_err("not there");
    assert!(
        gone.starts_with(&format!("{}: ", dir.join("gone").display())),
        "{gone}"
    );
}

/// A directory of the test to bind, and a file in it.
fn driver_dir(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let file = dir.join("libdriver.so.1");
    std::fs::write(&file, b"").expect("a file");
    (dir, file)
}

/// Proves each `--gpu-mount` that could expose or hide what it must not is refused naming the
/// flag and the value: not `SRC:DST`, a relative or missing source, a source below `/proc` (also
/// through a link), a source that is a file, a destination that is relative, climbs out or is
/// `/`, one below `/proc`, `/sys`, `/dev` or `/scratch`, and two mounts at one destination.
/// Catches a driver mount that reaches the kernel's trees, one that covers the job's scratch,
/// one that would make a file in the image's shared root, and two binds that shadow each other.
#[test]
fn a_mount_that_is_not_a_driver_directory_is_refused_naming_it() {
    let (dir, file) = driver_dir("gpu-mounts");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let src = source.display().to_string();
    let to_proc = dir.join("to-proc");
    std::os::unix::fs::symlink("/proc/self", &to_proc).expect("a link");
    let gone = dir.join("gone");
    let file = file.display().to_string();
    let link = to_proc.display().to_string();
    let missing = gone.display().to_string();
    let mut cases: Vec<(String, String)> = vec![
        ("no-colon".to_owned(), "is not SRC:DST".to_owned()),
        (
            "driver:/usr/local/driver".to_owned(),
            "the source driver is not an absolute path".to_owned(),
        ),
        (
            format!("{missing}:/usr/local/driver"),
            format!("the source {missing}: No such file or directory (os error 2)"),
        ),
        (
            "/proc/self:/usr/local/driver".to_owned(),
            format!("the source /proc/{} is below /proc", std::process::id()),
        ),
        (
            format!("{link}:/usr/local/driver"),
            format!("the source /proc/{} is below /proc", std::process::id()),
        ),
        (
            format!("{file}:/usr/local/driver"),
            format!(
                "the source {}/libdriver.so.1 is not a directory (a file bound where the image \
                 has none would be made in the image's root, which every job of the image shares)",
                source.display()
            ),
        ),
    ];
    let normal = "is not an absolute path without empty, `.` or `..` parts";
    for target in [
        "usr/local/driver",
        "/",
        "/usr//local",
        "/usr/./x",
        "/usr/../x",
    ] {
        cases.push((
            format!("{src}:{target}"),
            format!("the destination {target} {normal}"),
        ));
    }
    for (target, tree) in [
        ("/proc", "/proc"),
        ("/proc/driver", "/proc"),
        ("/sys/x", "/sys"),
        ("/dev/x", "/dev"),
        ("/scratch", "/scratch"),
        ("/scratch/lib", "/scratch"),
    ] {
        cases.push((
            format!("{src}:{target}"),
            format!("the destination {target} is below {tree}"),
        ));
    }
    for (value, why) in cases {
        let refused = refusal(&with_devices(&NVIDIA, &["--gpu-mount", &value]));
        assert_eq!(refused, format!("--gpu-mount: {value}: {why}"), "{value}");
    }
    let one = format!("{src}:/usr/local/driver");
    let two = with_devices(&NVIDIA, &["--gpu-mount", &one, "--gpu-mount", &one]);
    assert_eq!(
        refusal(&two),
        format!("--gpu-mount: {one}: another mount has the destination /usr/local/driver")
    );
}

/// Proves good mounts are kept read-only at their destinations, in order, bound from the path
/// the source resolves to (a link is resolved once, at start), and that a destination that
/// only begins like a refused tree (`/scratchpad`, `/device`) is a destination of its own.
/// Catches a writable driver mount, and a prefix test that refuses or allows by text.
#[test]
fn good_mounts_are_read_only_binds_of_the_resolved_source() {
    let (dir, _) = driver_dir("gpu-mounts-good");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&source, &link).expect("a link");
    let first = format!("{}:/scratchpad/driver", source.display());
    let second = format!("{}:/device", link.display());
    let flags = with_devices(&NVIDIA, &["--gpu-mount", &first, "--gpu-mount", &second]);
    let passthrough = checked(&flags).expect("good").expect("a device list");
    let bind = |target: &str| HostMount {
        target: target.to_owned(),
        source: source.clone(),
        read_only: true,
    };
    assert_eq!(
        passthrough.mounts,
        [bind("/scratchpad/driver"), bind("/device")]
    );
}

/// Proves each `--gpu-env` that is not a plain variable of the operator's is refused naming
/// the flag: not `NAME=VALUE`, a name outside `[A-Z_][A-Z0-9_]*` (lower case, a digit or a
/// dash first, empty), a name with the agent's `JS_` prefix, and a name given twice; and that
/// good ones are kept in order with a value that may hold `=`. Catches a variable that steers
/// the agent's own, and two values for one name of which the job sees either.
#[test]
fn a_variable_that_is_not_the_operators_is_refused_naming_it() {
    for (value, why) in [
        ("PATH", "--gpu-env: PATH is not NAME=VALUE".to_owned()),
        (
            "path=/x",
            r#"--gpu-env: "path" is not a name of [A-Z_][A-Z0-9_]*"#.to_owned(),
        ),
        (
            "1X=/x",
            r#"--gpu-env: "1X" is not a name of [A-Z_][A-Z0-9_]*"#.to_owned(),
        ),
        (
            "A-B=/x",
            r#"--gpu-env: "A-B" is not a name of [A-Z_][A-Z0-9_]*"#.to_owned(),
        ),
        (
            "=/x",
            r#"--gpu-env: "" is not a name of [A-Z_][A-Z0-9_]*"#.to_owned(),
        ),
        (
            "JS_RUN=x",
            "--gpu-env: JS_RUN is reserved: names that start with JS_ are the agent's".to_owned(),
        ),
    ] {
        let refused = refusal(&with_devices(&NVIDIA, &["--gpu-env", value]));
        assert_eq!(refused, why, "{value}");
    }
    let twice = with_devices(&NVIDIA, &["--gpu-env", "A=1", "--gpu-env", "A=2"]);
    assert_eq!(refusal(&twice), "--gpu-env: A is given twice");
    let good = with_devices(
        &NVIDIA,
        &[
            "--gpu-env",
            "LD_LIBRARY_PATH=/usr/local/driver/lib",
            "--gpu-env",
            "_X1=a=b",
        ],
    );
    let passthrough = checked(&good).expect("good").expect("a device list");
    let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        passthrough.env,
        [
            var("LD_LIBRARY_PATH", "/usr/local/driver/lib"),
            var("_X1", "a=b")
        ]
    );
}

/// The runtime's settings of the usual device list, a driver mount and a variable.
fn settings(mount: &str) -> GpuSettings {
    let flags = with_devices(
        &["/dev/nvidiactl", "/dev/nvidia{index}", "/dev/nvidia-uvm"],
        &[
            "--gpu-mount",
            mount,
            "--gpu-env",
            "A=gpu",
            "--gpu-env",
            "B=2",
        ],
    );
    let passthrough = checked(&flags).expect("good").expect("a device list");
    GpuSettings {
        passthrough,
        read: host_nodes,
    }
}

/// A container as the runtime makes it before a run's GPUs: one mount (the run's scratch),
/// the image's variables, no device.
fn container() -> ContainerSpec {
    ContainerSpec {
        name: "js-x-r1".to_owned(),
        image: format!("registry.example/job@sha256:{}", "ab".repeat(32)),
        image_digest: format!("sha256:{}", "ab".repeat(32)),
        dir: PathBuf::from("/s/js-x-r1"),
        cgroup_parent: "/actions/js-x-r1".to_owned(),
        exec_root: "/work".to_owned(),
        input_root: PathBuf::from("/s/js-x-r1/root"),
        upper: PathBuf::from("/s/js-x-r1/upper"),
        overlay_work: PathBuf::from("/s/js-x-r1/work"),
        working_directory: String::new(),
        env: vec![
            ("A".to_owned(), "image".to_owned()),
            ("C".to_owned(), "3".to_owned()),
        ],
        argv: vec!["/bin/tool".to_owned()],
        limits: ContainerLimits::DEFAULT,
        shared: Vec::new(),
        mounts: vec![HostMount {
            target: "/scratch".to_owned(),
            source: PathBuf::from("/scratch/r1"),
            read_only: false,
        }],
        devices: Vec::new(),
    }
}

/// The paths of the nodes `container` is given.
fn paths(container: &ContainerSpec) -> Vec<&str> {
    container.devices.iter().map(|d| d.path.as_str()).collect()
}

/// Proves a run that booked GPUs 3 and 1 is given the common nodes and exactly the nodes of
/// GPUs 3 and 1, each once, read when it starts; the driver mounts after its own; and the
/// variables, each over the image's of that name and after the image's others. Catches a run
/// given every GPU's node, or another index's, a node given twice, a mount placed before the
/// run's own, and an image variable that hides the operator's.
#[test]
fn a_run_is_given_the_nodes_of_its_booked_gpus_only() {
    let (dir, _) = driver_dir("gpu-wiring");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let gpus = settings(&format!("{}:/usr/local/driver", source.display()));
    let mut container = container();
    let own = container.mounts.clone();
    wiring(Some(&gpus), &[3, 1, 3])
        .expect("read")
        .apply(&mut container);
    assert_eq!(
        paths(&container),
        [
            "/dev/nvidiactl",
            "/dev/nvidia3",
            "/dev/nvidia1",
            "/dev/nvidia-uvm"
        ]
    );
    assert_eq!(container.devices[1].major, 195);
    assert_eq!(container.devices[1].minor, 3);
    assert_eq!(container.mounts[..own.len()], own[..]);
    assert_eq!(
        container.mounts[own.len()..],
        [HostMount {
            target: "/usr/local/driver".to_owned(),
            source,
            read_only: true,
        }]
    );
    let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        container.env,
        [var("A", "gpu"), var("C", "3"), var("B", "2")]
    );
}

/// Proves a run that booked no GPU is given nothing, whether the runtime has a device list or
/// not; that a run that booked one from a runtime without a list is refused with the words of
/// the `gpu-unsupported` refusal; and that a node gone since the start check refuses the run,
/// naming the node. Catches GPU wiring that leaks into other runs, a GPU run started without
/// its GPU, and a node that is skipped.
#[test]
fn a_run_without_gpus_is_given_nothing_and_a_missing_node_refuses_one_with_them() {
    let (dir, _) = driver_dir("gpu-wiring-none");
    let gpus = settings(&format!("{}:/usr/local/driver", dir.display()));
    for runtime in [None, Some(&gpus)] {
        let mut container = container();
        let before = container.clone();
        wiring(runtime, &[])
            .expect("nothing to read")
            .apply(&mut container);
        assert_eq!(container, before);
    }
    let refused = wiring(None, &[0]).expect_err("no device list");
    assert_eq!(refused, NO_GPUS);
    let gone = wiring(Some(&gpus), &[1, 9]).expect_err("a node is gone");
    assert_eq!(
        gone,
        "the GPU device /dev/nvidia9: No such file or directory"
    );
}

/// Proves the GPUs reach the crun settings and nothing else: crun settings carry the device
/// list they are given, and the settings of an engine are returned as they were. Catches a
/// device list lost between the flags and the runtime.
#[test]
fn the_device_list_is_kept_with_the_crun_settings_only() {
    let passthrough = checked(&with_devices(&NVIDIA, &[])).expect("good");
    let crun = Settings::for_crun("/usr/bin/crun".into(), "jsjob", (1, 1), Vec::new());
    let crun = crun.with_gpus(passthrough.clone());
    assert_eq!(crun.crun().expect("crun settings").gpus, passthrough);
    let exe = std::env::current_exe().expect("the test binary");
    let engine = Settings::new(&exe, None, None).expect("an engine's settings");
    assert_eq!(engine.clone().with_gpus(passthrough), engine);
}
