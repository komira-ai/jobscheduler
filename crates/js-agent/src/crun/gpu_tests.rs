//! The GPU flags of the crun runtime and what a run that booked GPUs is given. The host's
//! nodes are a table here ([`host_nodes`]): a test cannot make a character device without
//! root. The host's own reader is tested on `/dev/null`, which every Linux host has.

use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use clap::Parser as _;
use js_contain::container::{ContainerLimits, ContainerSpec, DeviceNode, HostMount};
use js_testkit::dir::TestDir;

use super::{
    DESTINATION_OFF_LIMITS, GpuPassthrough, GpuSettings, NO_GPUS, NodeError, Wiring, read_node,
    wiring,
};
use crate::Cli;
use crate::cli_runtime::Settings;

/// The indices of the GPUs of the host the tests check against.
const REPORTED: [u32; 4] = [0, 1, 2, 3];

/// The highest GPU index of [`host_nodes`]' table.
const LAST_GPU: u32 = 15;

/// What a missing node reads as.
fn missing(path: &Path) -> NodeError {
    NodeError::Unreadable {
        path: path.to_owned(),
        error: io::Error::from_raw_os_error(libc::ENOENT),
    }
}

/// A node of the table: device `major`:`minor`, root's, group 44, mode 0666.
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

/// The GPU index of `/dev/nvidia<n>`, for `n` of the table.
fn gpu_of(path: &Path) -> Option<u32> {
    path.to_str()
        .and_then(|text| text.strip_prefix("/dev/nvidia"))
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n <= LAST_GPU)
}

/// The nodes of a host with NVIDIA GPUs 0 to 15: `nvidiactl`, `nvidia-uvm` and `nvidia<n>`;
/// any other path is missing.
fn host_nodes(path: &Path) -> Result<DeviceNode, NodeError> {
    match (path.to_str(), gpu_of(path)) {
        (Some("/dev/nvidiactl"), _) => Ok(node_at(path, 195, 255)),
        (Some("/dev/nvidia-uvm"), _) => Ok(node_at(path, 511, 0)),
        (_, Some(n)) => Ok(node_at(path, 195, n)),
        _ => Err(missing(path)),
    }
}

/// A host with one GPU: `nvidiactl` and `nvidia0` only.
fn one_gpu_host(path: &Path) -> Result<DeviceNode, NodeError> {
    match path.to_str() {
        Some("/dev/nvidiactl" | "/dev/nvidia0") => host_nodes(path),
        _ => Err(missing(path)),
    }
}

/// The table's host after its nodes changed: `nvidia1` is a regular file now, and every
/// other node has other numbers, another owner and a narrower mode.
fn changed_nodes(path: &Path) -> Result<DeviceNode, NodeError> {
    if path == Path::new("/dev/nvidia1") {
        return Err(NodeError::NotCharacterDevice {
            path: path.to_owned(),
        });
    }
    host_nodes(path).map(|node| DeviceNode {
        major: node.major + 1,
        uid: 7,
        gid: 8,
        mode: 0o660,
        ..node
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

/// The checked flags `cli` on the host of the table with the GPUs [`REPORTED`].
fn checked(cli: &Cli) -> GpuPassthrough {
    let checked = GpuPassthrough::from_cli(cli, &REPORTED, host_nodes);
    checked.expect("good flags").expect("a device list")
}

/// The refusal of the flags `cli` on that host, as the agent prints it.
fn refusal(cli: &Cli) -> String {
    let refused = GpuPassthrough::from_cli(cli, &REPORTED, host_nodes);
    refused.expect_err("refused").to_string()
}

/// The usual device list: the control node and one node per GPU.
const NVIDIA: [&str; 2] = ["/dev/nvidiactl", "/dev/nvidia{index}"];

/// Proves an agent without GPU flags passes no GPU (the runtime keeps refusing GPU starts),
/// and that `--gpu-mount` and `--gpu-env` without a device list are refused naming the flag
/// and what it needs. Catches an agent that claims GPU support without a device list, and
/// mounts or variables that would silently go nowhere.
#[test]
fn without_a_device_list_no_gpu_is_passed_and_the_other_flags_are_refused() {
    let none = GpuPassthrough::from_cli(&cli(&[]), &REPORTED, host_nodes);
    assert!(none.expect("no flags").is_none());
    let mount = refusal(&cli(&["--gpu-mount", "/opt/driver:/usr/local/driver"]));
    assert_eq!(mount, "--gpu-mount: needs --gpu-device");
    let env = refusal(&cli(&["--gpu-env", "PATH=/x"]));
    assert_eq!(env, "--gpu-env: needs --gpu-device");
}

/// Proves a device list on a host that offers no GPU is refused (there is nothing to give a
/// job), and that only the GPUs the host offers are checked: a host that offers GPU 0 alone
/// (`--gpus 1` on a larger host gives the same list) passes with a node for GPU 0 only,
/// while the same nodes refuse an offer of GPUs 0 and 1, naming the node of GPU 1. Catches a
/// template checked against nothing, or against GPUs the agent does not offer.
#[test]
fn the_nodes_are_checked_for_exactly_the_gpus_the_host_offers() {
    let flags = with_devices(&NVIDIA, &[]);
    let none = GpuPassthrough::from_cli(&flags, &[], host_nodes).expect_err("no GPU");
    assert_eq!(
        none.to_string(),
        "--gpu-device: the host offers no GPU to give a job"
    );
    let one = GpuPassthrough::from_cli(&flags, &[0], one_gpu_host);
    assert!(one.expect("GPU 0's nodes are there").is_some());
    let two = GpuPassthrough::from_cli(&flags, &[0, 1], one_gpu_host).expect_err("GPU 1");
    assert_eq!(
        two.to_string(),
        "--gpu-device: /dev/nvidia1: No such file or directory (os error 2)"
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
/// common one, and a per-GPU one whose node is missing for one GPU the host reports (a
/// two-digit index among sparse ones). Catches a missing node skipped in silence (the job
/// would start without its GPU), and a template checked for the first GPU only.
#[test]
fn a_missing_node_is_refused_at_start_naming_it() {
    let common = refusal(&with_devices(&["/dev/nvidia-modeset"], &[]));
    assert_eq!(
        common,
        "--gpu-device: /dev/nvidia-modeset: No such file or directory (os error 2)"
    );
    let reported = [0, 12, 40];
    let gpu = GpuPassthrough::from_cli(&with_devices(&NVIDIA, &[]), &reported, host_nodes);
    assert_eq!(
        gpu.expect_err("GPU 40 has no node").to_string(),
        "--gpu-device: /dev/nvidia40: No such file or directory (os error 2)"
    );
}

/// Proves the host's reader gives a character device's numbers, owner and mode at the path
/// it was asked for, and refuses a regular file, a directory, a link to a character device
/// (it is not followed) and a path that is not there, naming the path and keeping the
/// system's reason as the source. Catches a reader that accepts any file, or follows a link
/// a job's writer could have placed.
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
        assert_eq!(
            why.to_string(),
            format!("{} is not a character device", path.display())
        );
    }
    let gone = dir.join("gone");
    let why = read_node(&gone).expect_err("not there");
    assert_eq!(
        why.to_string(),
        format!("{}: No such file or directory (os error 2)", gone.display())
    );
    let source = std::error::Error::source(&why).expect("the system's reason");
    assert_eq!(source.to_string(), "No such file or directory (os error 2)");
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
/// `/`, one at or below a kernel tree, the job's scratch or a place the runtime mounts itself
/// (`/work`, `/root`, `/tmp`, `/var/tmp`, `/run` with the run's files, the name files), or
/// one that holds such a place (`/etc`, `/var`).
/// Catches a driver mount that reaches the kernel's trees, one that covers the job's scratch,
/// home, overlay or files, and one that would make a file in the image's shared root.
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
    let pid = std::process::id();
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
            format!("the source /proc/{pid} is below /proc"),
        ),
        (
            format!("{link}:/usr/local/driver"),
            format!("the source /proc/{pid} is below /proc"),
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
        "/usr/local/",
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
        ("/work/driver", "/work"),
        ("/root/driver", "/root"),
        ("/tmp/driver", "/tmp"),
        ("/var/tmp/driver", "/var/tmp"),
        ("/run/js-files/driver", "/run"),
        ("/etc/hosts", "/etc/hosts"),
        ("/etc/hostname", "/etc/hostname"),
        ("/etc/resolv.conf", "/etc/resolv.conf"),
    ] {
        cases.push((
            format!("{src}:{target}"),
            format!("the destination {target} is below {tree}"),
        ));
    }
    for (target, tree) in [("/etc", "/etc/hosts"), ("/var", "/var/tmp")] {
        cases.push((
            format!("{src}:{target}"),
            format!("the destination {target} holds {tree}, which the driver directory would hide"),
        ));
    }
    for (value, why) in cases {
        let refused = refusal(&with_devices(&NVIDIA, &["--gpu-mount", &value]));
        assert_eq!(refused, format!("--gpu-mount: {value}: {why}"), "{value}");
    }
    assert_eq!(DESTINATION_OFF_LIMITS.len(), 12);
}

/// Proves two mounts whose destinations are the same, or one below the other (in either
/// order), are refused naming both: a later bind would cover the earlier, or be made inside
/// the operator's read-only directory. A destination that only begins like another
/// (`/usr/local/driverx`) is a destination of its own. Catches two binds that shadow each
/// other, and a prefix test that compares text.
#[test]
fn mounts_that_overlap_are_refused_naming_both() {
    let (dir, _) = driver_dir("gpu-mounts-overlap");
    let src = std::fs::canonicalize(dir.path()).expect("canonical");
    let at = |target: &str| format!("{}:{target}", src.display());
    for (first, second) in [
        ("/usr/local/driver", "/usr/local/driver"),
        ("/usr/local/driver", "/usr/local/driver/lib"),
        ("/usr/local/driver/lib", "/usr/local/driver"),
    ] {
        let (one, two) = (at(first), at(second));
        let flags = with_devices(&NVIDIA, &["--gpu-mount", &one, "--gpu-mount", &two]);
        assert_eq!(
            refusal(&flags),
            format!(
                "--gpu-mount: {two}: the destination {second} overlaps {first}, another mount's"
            ),
            "{first} then {second}"
        );
    }
    let (one, two) = (at("/usr/local/driver"), at("/usr/local/driverx"));
    let flags = with_devices(&NVIDIA, &["--gpu-mount", &one, "--gpu-mount", &two]);
    assert_eq!(checked(&flags).mounts.len(), 2);
}

/// Proves good mounts are kept read-only and not recursive at their destinations, in order,
/// bound from the path the source resolves to (a link is resolved once, at start), and that
/// a destination that only begins like a refused tree (`/scratchpad`, `/device`, `/runner`,
/// `/etc/hosts.d`) is a destination of its own. Catches a writable driver mount, a recursive
/// one (a writable mount below the directory would reach the job), and a prefix test that
/// refuses or allows by text.
#[test]
fn good_mounts_are_read_only_binds_of_the_resolved_source() {
    let (dir, _) = driver_dir("gpu-mounts-good");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&source, &link).expect("a link");
    let targets = ["/scratchpad/driver", "/device", "/runner", "/etc/hosts.d"];
    let values: Vec<String> = targets
        .iter()
        .enumerate()
        .map(|(n, target)| match n {
            0 => format!("{}:{target}", source.display()),
            _ => format!("{}:{target}", link.display()),
        })
        .collect();
    let mut flags: Vec<&str> = Vec::new();
    for value in &values {
        flags.extend(["--gpu-mount", value]);
    }
    let passthrough = checked(&with_devices(&NVIDIA, &flags));
    let bind = |target: &str| HostMount {
        target: target.to_owned(),
        source: source.clone(),
        read_only: true,
        recursive: false,
        no_exec: false,
    };
    let want: Vec<HostMount> = targets.iter().map(|target| bind(target)).collect();
    assert_eq!(passthrough.mounts, want);
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
    let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        checked(&good).env,
        [
            var("LD_LIBRARY_PATH", "/usr/local/driver/lib"),
            var("_X1", "a=b")
        ]
    );
}

/// The runtime's settings of `devices`, a driver mount from `mount` and two variables, read
/// at the runs' starts by `read`.
fn settings_of(devices: &[&str], mount: &str, read: super::ReadNode) -> GpuSettings {
    let mut flags = vec!["--gpu-env", "PATH=/gpu/bin", "--gpu-env", "B=2"];
    if !mount.is_empty() {
        flags.extend(["--gpu-mount", mount]);
    }
    GpuSettings {
        passthrough: checked(&with_devices(devices, &flags)),
        read,
    }
}

/// A container as the runtime makes it before a run's GPUs: one mount (the run's scratch),
/// the image's variables (which name `PATH` twice), no device.
fn container() -> ContainerSpec {
    let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    ContainerSpec {
        name: "js-x-r1".to_owned(),
        image: format!("registry.example/job@sha256:{}", "ab".repeat(32)),
        image_digest: format!("sha256:{}", "ab".repeat(32)),
        dir: PathBuf::from("/s/js-x-r1"),
        cgroup_parent: "/actions/js-x-r1".to_owned(),
        isolation: js_contain::Isolation::Contained,
        exec_root: "/work".to_owned(),
        input_root: PathBuf::from("/s/js-x-r1/root"),
        upper: PathBuf::from("/s/js-x-r1/upper"),
        overlay_work: PathBuf::from("/s/js-x-r1/work"),
        working_directory: String::new(),
        env: vec![
            var("PATH", "/image/first"),
            var("C", "3"),
            var("PATH", "/image/second"),
        ],
        argv: vec!["/bin/tool".to_owned()],
        limits: ContainerLimits::DEFAULT,
        shared: Vec::new(),
        mounts: vec![HostMount {
            target: "/scratch".to_owned(),
            source: PathBuf::from("/scratch/r1"),
            read_only: false,
            recursive: true,
            no_exec: false,
        }],
        devices: Vec::new(),
        mounted_root: None,
        listen_ports: Vec::new(),
    }
}

/// The paths of the nodes `container` is given for a run of `gpus` that booked `indices`.
fn given(gpus: &GpuSettings, indices: &[u32]) -> Vec<String> {
    let mut container = container();
    wiring(Some(gpus), indices)
        .expect("read")
        .apply(&mut container);
    container.devices.into_iter().map(|d| d.path).collect()
}

/// Proves a run is given exactly the nodes of the indices it booked, for every shape of a
/// device list: the common nodes and per-GPU nodes in the order listed and booked, each once
/// (an index booked twice, or a common node that a template also names, gives one node);
/// a list of common nodes only gives those whatever was booked; a list of templates only
/// gives one node per booked index, sparse and two-digit ones too. Catches a run given every
/// GPU's node, another index's, a node given twice, or a two-digit index cut to one digit.
#[test]
fn a_run_is_given_the_nodes_of_its_booked_gpus_only() {
    let both = ["/dev/nvidiactl", "/dev/nvidia{index}", "/dev/nvidia-uvm"];
    for (devices, booked, want) in [
        (
            &both[..],
            &[3, 1, 3][..],
            &[
                "/dev/nvidiactl",
                "/dev/nvidia3",
                "/dev/nvidia1",
                "/dev/nvidia-uvm",
            ][..],
        ),
        (
            &["/dev/nvidiactl", "/dev/nvidia-uvm"][..],
            &[2][..],
            &["/dev/nvidiactl", "/dev/nvidia-uvm"][..],
        ),
        (
            &["/dev/nvidia{index}"][..],
            &[12, 0, 7][..],
            &["/dev/nvidia12", "/dev/nvidia0", "/dev/nvidia7"][..],
        ),
        (
            &["/dev/nvidia1", "/dev/nvidia{index}"][..],
            &[1, 2][..],
            &["/dev/nvidia1", "/dev/nvidia2"][..],
        ),
    ] {
        let gpus = settings_of(devices, "", host_nodes);
        assert_eq!(given(&gpus, booked), want, "{devices:?} {booked:?}");
    }
}

/// Proves what a GPU run is given besides its nodes: its numbers read when it starts (not
/// those of the start check); the driver mounts after the run's own; and each variable set
/// once, over the image's (the image's two `PATH` entries become the operator's one), the
/// image's others kept. Catches nodes kept from the start, a mount placed before the run's
/// own, and an image entry left that a program reading the last one would take.
#[test]
fn a_run_is_given_its_nodes_as_they_are_now_and_the_operators_variables() {
    let (dir, _) = driver_dir("gpu-wiring");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let mount = format!("{}:/usr/local/driver", source.display());
    let gpus = settings_of(&NVIDIA, &mount, changed_nodes);
    let mut container = container();
    let own = container.mounts.clone();
    wiring(Some(&gpus), &[3])
        .expect("read")
        .apply(&mut container);
    let nodes: Vec<(&str, u32, u32, u32, u32, u32)> = container
        .devices
        .iter()
        .map(|d| (d.path.as_str(), d.major, d.minor, d.uid, d.gid, d.mode))
        .collect();
    assert_eq!(
        nodes,
        [
            ("/dev/nvidiactl", 196, 255, 7, 8, 0o660),
            ("/dev/nvidia3", 196, 3, 7, 8, 0o660)
        ]
    );
    assert_eq!(container.mounts[..own.len()], own[..]);
    assert_eq!(
        container.mounts[own.len()..],
        [HostMount {
            target: "/usr/local/driver".to_owned(),
            source,
            read_only: true,
            recursive: false,
            no_exec: false,
        }]
    );
    let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        container.env,
        [var("PATH", "/gpu/bin"), var("C", "3"), var("B", "2")]
    );
}

/// Proves a run that booked no GPU is given nothing, whether the runtime has a device list or
/// not; that a run that booked one from a runtime without a list is refused with the words of
/// the `gpu-unsupported` refusal; that a node gone since the start check, or one that is now
/// a regular file, refuses the run naming the node; and that a driver directory removed, or
/// replaced by a file, since the start check refuses it naming `--gpu-mount`, and the run
/// is given its GPUs again once the directory is back. Catches GPU wiring that leaks into
/// other runs, a GPU run started without its GPU or its driver, and a node that is skipped.
#[test]
fn a_run_is_refused_when_its_nodes_or_driver_are_gone_and_nothing_given_without_gpus() {
    let (dir, _) = driver_dir("gpu-wiring-none");
    let source = std::fs::canonicalize(dir.path()).expect("canonical");
    let mount = format!("{}:/usr/local/driver", source.display());
    let gpus = settings_of(&NVIDIA, &mount, host_nodes);
    for runtime in [None, Some(&gpus)] {
        let mut container = container();
        let before = container.clone();
        wiring(runtime, &[])
            .expect("nothing to read")
            .apply(&mut container);
        assert_eq!(container, before);
    }
    let refused = wiring(None, &[0]).expect_err("no device list");
    assert_eq!(refused.to_string(), NO_GPUS);
    let gone = wiring(Some(&gpus), &[1, 40]).expect_err("a node is gone");
    assert_eq!(
        gone.to_string(),
        "the GPU device /dev/nvidia40: No such file or directory (os error 2)"
    );
    let changed = settings_of(&NVIDIA, &mount, changed_nodes);
    let file = wiring(Some(&changed), &[1]).expect_err("a file now");
    assert_eq!(
        file.to_string(),
        "the GPU device /dev/nvidia1 is not a character device"
    );
    let shown = source.display();
    std::fs::remove_dir_all(&source).expect("the driver goes");
    let removed = wiring(Some(&gpus), &[0]).expect_err("the driver is gone");
    assert_eq!(
        removed.to_string(),
        format!(
            "--gpu-mount: the source {shown} is not a directory now: No such file or \
             directory (os error 2)"
        )
    );
    std::fs::write(&source, b"").expect("a file in its place");
    let replaced = wiring(Some(&gpus), &[0]).expect_err("a file is there");
    assert_eq!(
        replaced.to_string(),
        format!("--gpu-mount: the source {shown} is not a directory now: something else is there")
    );
    std::fs::remove_file(&source).expect("the file goes");
    std::fs::create_dir(&source).expect("the driver is back");
    assert_eq!(given(&gpus, &[0]), ["/dev/nvidiactl", "/dev/nvidia0"]);
}

/// The wiring of driver mounts at `targets`.
fn mounts_at(targets: &[&str]) -> Wiring {
    Wiring {
        mounts: targets
            .iter()
            .map(|target| HostMount {
                target: (*target).to_owned(),
                source: PathBuf::from("/opt/driver"),
                read_only: true,
                recursive: false,
                no_exec: false,
            })
            .collect(),
        ..Wiring::default()
    }
}

/// The refusal of the driver mount at `target` in the image root `rootfs`.
fn target_refusal(rootfs: &Path, target: &str) -> String {
    let refused = mounts_at(&[target]).check_targets(rootfs);
    refused.expect_err(target).to_string()
}

/// Proves a driver mount's destination is walked in the image's root as the container would
/// see it and never followed out of it: one the image lacks (or lacks below a directory it
/// has) and one that is a directory are fine; a link anywhere on the way (to `/scratch`, to
/// another directory, to itself) is refused, so no destination is redirected and no two
/// alias; a file on the way or at the end is refused; a part that cannot be looked up
/// (a directory the agent may not search) and a root that is not there are refused with
/// the system's reason. Each refusal names `--gpu-mount`, the destination and the part.
/// Catches a bind that a hostile image redirects onto the job's scratch or `/dev`, two
/// destinations that land on one directory, and a lookup failure taken for a missing part.
#[test]
fn a_destination_is_walked_in_the_image_without_following_a_link() {
    let dir = TestDir::under(std::env::temp_dir(), "gpu-walk");
    let root = dir.join("rootfs");
    for made in ["usr/local/nvidia", "opt/real", "locked"] {
        std::fs::create_dir_all(root.join(made)).expect("a directory of the image");
    }
    std::fs::write(root.join("usr/lib"), b"").expect("a file of the image");
    for (link, to) in [
        ("driver", "/scratch"),
        ("usr/alias", "/opt/real"),
        ("loop", "loop"),
    ] {
        std::os::unix::fs::symlink(to, root.join(link)).expect("a link of the image");
    }
    let fine = [
        "/usr/local/nvidia",
        "/usr/local/nvidia/lib64",
        "/opt/new/lib",
    ];
    mounts_at(&fine).check_targets(&root).expect("fine");
    let refusals = [
        (
            "/driver/lib",
            "the image has a link at /driver, which a driver mount does not follow",
        ),
        (
            "/usr/alias",
            "the image has a link at /usr/alias, which a driver mount does not follow",
        ),
        (
            "/loop/x",
            "the image has a link at /loop, which a driver mount does not follow",
        ),
        (
            "/usr/lib/nvidia",
            "the image has something other than a directory at /usr/lib",
        ),
        (
            "/usr/lib",
            "the image has something other than a directory at /usr/lib",
        ),
    ];
    for (target, why) in refusals {
        assert_eq!(
            target_refusal(&root, target),
            format!("--gpu-mount: the destination {target}: {why}"),
            "{target}"
        );
    }
    let locked = root.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock it");
    let denied = target_refusal(&root, "/locked/driver");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("unlock it");
    assert_eq!(
        denied,
        "--gpu-mount: the destination /locked/driver: /locked/driver cannot be looked up in \
         the image: Permission denied (os error 13)"
    );
    assert_eq!(
        target_refusal(&dir.join("gone"), "/usr/local/nvidia"),
        "--gpu-mount: the destination /usr/local/nvidia: / cannot be looked up in the image: \
         No such file or directory (os error 2)"
    );
}

/// Proves the GPUs reach the crun settings and nothing else: crun settings carry the device
/// list they are given, and the settings of an engine are returned as they were. Catches a
/// device list lost between the flags and the runtime.
#[test]
fn the_device_list_is_kept_with_the_crun_settings_only() {
    let passthrough = Some(checked(&with_devices(&NVIDIA, &[])));
    let crun = Settings::for_crun("/usr/bin/crun".into(), "jsjob", (1, 1), Vec::new());
    let crun = crun.with_gpus(passthrough.clone());
    assert_eq!(crun.crun().expect("crun settings").gpus, passthrough);
    let exe = std::env::current_exe().expect("the test binary");
    let engine = Settings::new(&exe, None).expect("an engine's settings");
    assert_eq!(engine.clone().with_gpus(passthrough), engine);
}
