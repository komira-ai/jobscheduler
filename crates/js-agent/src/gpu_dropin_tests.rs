//! The GPU drop-in of the crun unit, made from a fake `/dev` in a test directory. A test cannot
//! make a character device without root, so the check reads the nodes through a table
//! ([`any_node`]) and the files below the fake `/dev` are plain files: the verb lists names.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;
use js_contain::container::DeviceNode;
use js_testkit::dir::TestDir;

use super::{
    Args, BIN_TARGET, Host, LIB_TARGET, Nodes, dropin, exec_start, flags, gpu_index, listed_index,
    run,
};
use crate::Cli;
use crate::crun::{GpuPassthrough, NodeError};
use crate::flags_file::{FileFacts, expand_with};

/// The packaged crun unit, whose `ExecStart` the drop-in extends.
const PACKAGED_UNIT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packaging/js-agent/js-agent-crun.service"
);

/// The unit hostkeeper's package links, whose `ExecStart` names the operator's flags file.
const HOSTKEEPER_UNIT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packaging/js-agent/hostkeeper/units/js-agent-crun.service"
);

/// The unit most tests extend.
const UNIT: &str = "[Unit]\nDescription=x\nExecStart=/not/a/service/line\n\n[Service]\n\
    Type=simple\nExecStart=/opt/js-agent/link/current/js-agent --server https://s.example:7300 \
    --token-file /etc/js-agent/token --pool p --runtime crun\nKillMode=process\n";

/// The command line of [`UNIT`].
const UNIT_LINE: &str = "/opt/js-agent/link/current/js-agent --server https://s.example:7300 \
    --token-file /etc/js-agent/token --pool p --runtime crun";

/// Any `/dev` path reads as a node of the NVIDIA driver's major number.
fn any_node(path: &Path) -> Result<DeviceNode, NodeError> {
    Ok(DeviceNode {
        path: path.to_string_lossy().into_owned(),
        major: 195,
        minor: 0,
        uid: 0,
        gid: 0,
        mode: 0o666,
    })
}

/// Every path reads as something other than a character device.
fn no_node(path: &Path) -> Result<DeviceNode, NodeError> {
    Err(NodeError::NotCharacterDevice {
        path: path.to_owned(),
    })
}

/// An `nvidia-smi` that cannot be run.
fn absent_smi(program: &str) -> Result<String, String> {
    Err(format!("{program}: not found"))
}

/// The `-L` list of a host with two GPUs.
fn two_gpus(_: &str) -> Result<String, String> {
    Ok("GPU 0: NVIDIA A100 (UUID: GPU-a)\nGPU 1: NVIDIA A100 (UUID: GPU-b)\n".to_owned())
}

/// What `nvidia-smi -L` prints on a host whose driver sees no GPU.
fn no_gpus(_: &str) -> Result<String, String> {
    Ok("No devices were found\n".to_owned())
}

/// The `-L` list of a host with three GPUs.
fn three_gpus(_: &str) -> Result<String, String> {
    Ok("GPU 0: A (UUID: GPU-a)\nGPU 1: A (UUID: GPU-b)\nGPU 2: A (UUID: GPU-c)\n".to_owned())
}

/// How a run ended and what it printed.
struct Ran {
    code: String,
    out: String,
    err: String,
}

/// The `Debug` text of exit status `n`, which is how two statuses are compared.
fn status(n: u8) -> String {
    format!("{:?}", ExitCode::from(n))
}

/// A fake host: a `/dev`, a driver directory with `lib64` and `bin`, the unit and where the
/// drop-in goes.
struct Rig {
    dir: TestDir,
    dev: PathBuf,
    driver: PathBuf,
    unit: PathBuf,
    out: PathBuf,
    /// The text every `--flags-file` reads as, root's and mode 0600; none is a missing file.
    flags: Option<&'static str>,
}

impl Rig {
    fn new(name: &str, nodes: &[&str]) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let dev = dir.join("dev");
        let driver = dir.join("nvidia-driver");
        for sub in ["dev", "nvidia-driver/lib64", "nvidia-driver/bin", "etc"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for node in nodes {
            let path = dev.join(node);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let unit = dir.join("etc/js-agent-crun.service");
        std::fs::write(&unit, UNIT).unwrap();
        let out = dir.join("run/js-agent-crun.service.d/zz-gpu.conf");
        Self {
            dir,
            dev,
            driver,
            unit,
            out,
            flags: None,
        }
    }

    /// The verb's arguments for this rig, then `extra`.
    fn args(&self, extra: &[&str]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["js-agent".into(), "crun-gpu-dropin".into()];
        for (flag, value) in [
            ("--out", &self.out),
            ("--unit", &self.unit),
            ("--dev-root", &self.dev),
            ("--driver-root", &self.driver),
        ] {
            args.push(flag.into());
            args.push(value.into());
        }
        args.extend(extra.iter().map(OsString::from));
        args
    }

    fn run_with(&self, extra: &[&str], smi: fn(&str) -> Result<String, String>) -> Ran {
        self.run_host(&self.args(extra), smi, any_node)
    }

    fn run_host(
        &self,
        args: &[OsString],
        smi: fn(&str) -> Result<String, String>,
        read: fn(&Path) -> Result<DeviceNode, NodeError>,
    ) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let file = |_: &Path| match self.flags {
            Some(text) => Ok(FileFacts {
                regular: true,
                mode: 0o600,
                uid: 0,
                bytes: text.as_bytes().to_vec(),
            }),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        };
        let expand = |args: Vec<OsString>| expand_with(args, 0, &file);
        let host = Host {
            smi: &smi,
            read,
            expand: &expand,
        };
        let code = run(args, &mut out, &mut err, &host);
        Ran {
            code: format!("{code:?}"),
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// The drop-in's command line, read back as systemd reads the unit and the drop-in.
    fn written_line(&self, unit: &str) -> String {
        let text = std::fs::read_to_string(&self.out).unwrap();
        let service: Vec<&str> = text.lines().skip_while(|l| *l != "[Service]").collect();
        assert_eq!(service.len(), 3, "{text}");
        assert_eq!(service[1], "ExecStart=", "the reset comes first: {text}");
        assert!(service[2].starts_with("ExecStart="), "{text}");
        exec_start([unit, text.as_str()]).unwrap()
    }

    fn driver_flags(&self) -> Vec<String> {
        let lib = self.driver.join("lib64");
        let bin = self.driver.join("bin");
        [
            "--gpu-mount".to_owned(),
            format!("{}:{LIB_TARGET}", lib.display()),
            "--gpu-mount".to_owned(),
            format!("{}:{BIN_TARGET}", bin.display()),
            "--gpu-env".to_owned(),
            format!("LD_LIBRARY_PATH={LIB_TARGET}"),
            "--gpu-env".to_owned(),
            format!(
                "PATH={BIN_TARGET}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            ),
        ]
        .to_vec()
    }
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// Two GPUs, `nvidiactl` and `nvidia-uvm` (and other nodes the driver does not own) give the
/// per-GPU node once and each common node once, in name order; the drop-in's command line is
/// the unit's with those flags, and it checks to the same value as the same flags written by
/// hand.
#[test]
fn two_gpus_give_the_index_node_and_each_common_node_in_name_order() {
    let rig = Rig::new(
        "dropin-two",
        &[
            "nvidia1",
            "nvidiactl",
            "nvidia0",
            "nvidia-uvm",
            "sda",
            "null",
        ],
    );
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let mut expected = words(
        "--gpu-device /dev/nvidia{index} --gpu-device /dev/nvidia-uvm \
         --gpu-device /dev/nvidiactl",
    );
    expected.extend(rig.driver_flags());
    let line = rig.written_line(UNIT);
    let mut want = words(UNIT_LINE);
    want.extend(expected.iter().cloned());
    assert_eq!(words(&line), want);
    assert_eq!(
        ran.out,
        format!(
            "crun-gpu-dropin: wrote {} for 2 GPUs: {}\n",
            rig.out.display(),
            expected.join(" ")
        )
    );
    assert_eq!(ran.err, "");
    let cli = Cli::try_parse_from(words(&line)).unwrap();
    assert_eq!(
        cli.gpu_devices,
        ["/dev/nvidia{index}", "/dev/nvidia-uvm", "/dev/nvidiactl"]
    );
    let checked = GpuPassthrough::from_cli(&cli, &[0, 1], any_node).unwrap();
    assert!(checked.is_some(), "the flags check to a device list");
}

/// The pure step: names to nodes to flags, the same for any order of the listing.
#[test]
fn the_flags_depend_on_the_names_and_not_their_order() {
    let names: Vec<String> = ["nvidia2", "nvidia-modeset", "nvidia0", "nvidia-uvm-tools"]
        .map(str::to_owned)
        .to_vec();
    let mut reversed = names.clone();
    reversed.reverse();
    let caps = ["nvidia-cap2".to_owned(), "nvidia-cap1".to_owned()];
    let nodes = Nodes::from_names(&names, &caps);
    assert_eq!(nodes, Nodes::from_names(&reversed, &caps));
    assert_eq!(nodes.gpus.iter().copied().collect::<Vec<_>>(), [0, 2]);
    let got = flags(&nodes, Path::new("/opt/d"));
    let want = words(
        "--gpu-device /dev/nvidia{index} --gpu-device /dev/nvidia-caps/nvidia-cap1 \
         --gpu-device /dev/nvidia-caps/nvidia-cap2 --gpu-device /dev/nvidia-modeset \
         --gpu-device /dev/nvidia-uvm-tools \
         --gpu-mount /opt/d/lib64:/usr/local/nvidia/lib64 \
         --gpu-mount /opt/d/bin:/usr/local/nvidia/bin \
         --gpu-env LD_LIBRARY_PATH=/usr/local/nvidia/lib64 \
         --gpu-env PATH=/usr/local/nvidia/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    assert_eq!(got, want);
}

/// The entries of `nvidia-caps/` are given, each as its own node.
#[test]
fn the_capability_nodes_are_given() {
    let rig = Rig::new(
        "dropin-caps",
        &[
            "nvidia0",
            "nvidiactl",
            "nvidia-caps/nvidia-cap1",
            "nvidia-caps/nvidia-cap2",
        ],
    );
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let line = words(&rig.written_line(UNIT));
    for cap in [
        "/dev/nvidia-caps/nvidia-cap1",
        "/dev/nvidia-caps/nvidia-cap2",
    ] {
        let at = line.iter().position(|w| w == cap).unwrap();
        assert_eq!(line[at - 1], "--gpu-device");
    }
}

/// `nvidia<N>` is a GPU only with `N` in plain decimal.
#[test]
fn only_a_decimal_index_names_a_gpu() {
    assert_eq!(gpu_index("nvidia7"), Some(7));
    assert_eq!(gpu_index("nvidia12"), Some(12));
    for name in [
        "nvidia",
        "nvidia03",
        "nvidiactl",
        "nvidia-uvm",
        "nvidia-nvswitch0",
        "sda",
    ] {
        assert_eq!(gpu_index(name), None, "{name}");
    }
}

/// A host with no GPU node (an empty `/dev`, or only nodes of other devices and of a driver
/// without a GPU) gets no drop-in, exits 0, and loses one an earlier run wrote.
#[test]
fn a_host_without_a_gpu_node_gets_no_dropin_and_exits_0() {
    for (name, nodes) in [
        ("dropin-empty", &[][..]),
        (
            "dropin-other",
            &["sda", "nvme0", "nvidia03", "nvidiactl", "nvidia-nvswitch0"][..],
        ),
    ] {
        let rig = Rig::new(name, nodes);
        std::fs::create_dir_all(rig.out.parent().unwrap()).unwrap();
        std::fs::write(&rig.out, "stale").unwrap();
        let ran = rig.run_with(&[], absent_smi);
        assert_eq!(ran.code, status(0), "{name}");
        assert!(
            !rig.out.exists(),
            "{name}: the earlier drop-in is still there"
        );
        assert_eq!(
            ran.out,
            format!(
                "crun-gpu-dropin: no GPU node in {}: no drop-in written\n",
                rig.dev.display()
            )
        );
        assert_eq!(ran.err, "");
    }
}

/// Flags the check refuses (the driver's `lib64` is missing) write nothing, remove an earlier
/// drop-in, and exit 1 with the check's own reason.
#[test]
fn flags_the_check_refuses_write_nothing_and_exit_1() {
    let rig = Rig::new("dropin-refused", &["nvidia0", "nvidiactl"]);
    std::fs::remove_dir(rig.driver.join("lib64")).unwrap();
    std::fs::create_dir_all(rig.out.parent().unwrap()).unwrap();
    std::fs::write(&rig.out, "stale").unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    let lib = rig.driver.join("lib64");
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: --gpu-mount: {lib}:/usr/local/nvidia/lib64: the source {lib}: \
             No such file or directory (os error 2); no drop-in written\n",
            lib = lib.display()
        )
    );
    assert_eq!(ran.out, "");
    assert!(!rig.out.exists());
    let temp = rig.out.with_extension("conf.new");
    assert!(!temp.exists(), "a partial file is left");
}

/// A node the check cannot read as a character device is refused the same way. The node is
/// read where it was listed, in the dev root: with the host's own reader, the fake `/dev`'s plain
/// files are refused by their path there.
#[test]
fn a_node_that_is_not_a_device_is_refused() {
    let rig = Rig::new("dropin-not-node", &["nvidia0"]);
    for read in [no_node, crate::crun::read_node] {
        let ran = rig.run_host(&rig.args(&[]), absent_smi, read);
        assert_eq!(ran.code, status(1));
        assert_eq!(
            ran.err,
            format!(
                "crun-gpu-dropin: --gpu-device: {} is not a character device; no drop-in \
                 written\n",
                rig.dev.join("nvidia0").display()
            )
        );
        assert!(!rig.out.exists());
    }
}

/// `nvidia-smi -L` listing another number of GPUs than the nodes is a warning: the drop-in is
/// still written. The same number draws none; an `nvidia-smi` that cannot be run is a warning.
#[test]
fn a_gpu_count_that_differs_from_nvidia_smi_is_a_warning() {
    let rig = Rig::new("dropin-smi", &["nvidia0", "nvidia1", "nvidiactl"]);
    let ran = rig.run_with(&["--nvidia-smi", "/usr/bin/nvidia-smi"], three_gpus);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    assert!(rig.out.exists());
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: warning: /usr/bin/nvidia-smi -L lists the GPUs {{0, 1, 2}} and {} \
             has the GPU nodes {{0, 1}}\n",
            rig.dev.display()
        )
    );

    let ran = rig.run_with(&["--nvidia-smi", "/usr/bin/nvidia-smi"], two_gpus);
    assert_eq!(ran.code, status(0));
    assert_eq!(ran.err, "");

    let ran = rig.run_with(&["--nvidia-smi", "/usr/bin/nvidia-smi"], absent_smi);
    assert_eq!(ran.code, status(0));
    assert_eq!(
        ran.err,
        "crun-gpu-dropin: warning: /usr/bin/nvidia-smi -L: /usr/bin/nvidia-smi: not found; the \
         flags are checked against the nodes\n"
    );
    assert!(rig.out.exists());
}

/// The drop-in extends the packaged unit's own `ExecStart`: read back as systemd reads the unit
/// and the drop-in, its command line is the unit's, word for word, followed by the GPU flags,
/// and the agent's parser accepts it. A change to the unit's `ExecStart` is followed by the
/// drop-in.
#[test]
fn the_dropin_extends_the_packaged_units_exec_start() {
    let rig = Rig::new("dropin-packaged", &["nvidia0", "nvidiactl", "nvidia-uvm"]);
    let packaged = std::fs::read_to_string(PACKAGED_UNIT).unwrap();
    std::fs::write(&rig.unit, &packaged).unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let own = exec_start([packaged.as_str()]).unwrap();
    assert!(
        own.starts_with("/opt/js-agent/link/current/js-agent "),
        "{own}"
    );
    let line = rig.written_line(&packaged);
    let mut want = words(&own);
    want.extend(words(
        "--gpu-device /dev/nvidia{index} --gpu-device /dev/nvidia-uvm \
         --gpu-device /dev/nvidiactl",
    ));
    want.extend(rig.driver_flags());
    assert_eq!(words(&line), want);
    let cli = Cli::try_parse_from(words(&line)).unwrap();
    assert_eq!(cli.gpu_devices.len(), 3);
}

/// The unit's drop-ins (the `systemctl edit` override in /etc, and the runtime ones beside the
/// verb's own) are read in name order and the last `ExecStart` wins, the unit's directory winning
/// for a name in both; the verb's own drop-in and files that are not `.conf` are not read.
#[test]
fn the_units_own_dropins_are_read_but_not_the_verbs() {
    let rig = Rig::new("dropin-override", &["nvidia0"]);
    let dir = rig.dir.join("etc/js-agent-crun.service.d");
    std::fs::create_dir_all(&dir).unwrap();
    let override_line = "/opt/js-agent/link/current/js-agent --server https://o.example:7300 \
                         --token-file /t --pool o";
    std::fs::write(
        dir.join("override.conf"),
        format!("[Service]\nExecStart=\nExecStart={override_line}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("10-first.conf"), "[Service]\nExecStart=/bogus\n").unwrap();
    std::fs::write(dir.join("zz-gpu.conf"), "[Service]\nExecStart=/bogus\n").unwrap();
    std::fs::write(dir.join("zz-notes.txt"), "[Service]\nExecStart=/bogus\n").unwrap();
    let runtime = rig.out.parent().unwrap().to_owned();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(
        runtime.join("override.conf"),
        "[Service]\nExecStart=\nExecStart=/bogus\n",
    )
    .unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let text = std::fs::read_to_string(&rig.out).unwrap();
    let line = exec_start([text.as_str()]).unwrap();
    assert!(
        line.starts_with(&format!("{override_line} --gpu-device")),
        "{line}"
    );

    let runtime_line = "/opt/js-agent/link/current/js-agent --server https://r.example:7300 \
                        --token-file /t --pool r";
    std::fs::write(
        runtime.join("runtime.conf"),
        format!("[Service]\nExecStart=\nExecStart={runtime_line}\n"),
    )
    .unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let text = std::fs::read_to_string(&rig.out).unwrap();
    let line = exec_start([text.as_str()]).unwrap();
    assert!(
        line.starts_with(&format!("{runtime_line} --gpu-device")),
        "{line}"
    );
}

/// The check runs against the GPUs `nvidia-smi -L` lists, which are the ones the agent checks
/// at start: a GPU it lists with no node is refused (nothing written, exit 1), even where `/dev`
/// has no GPU node at all; and a host with GPU nodes whose `nvidia-smi` lists no GPU is refused
/// too (the agent would offer no GPU and refuse the flags at start). A MIG device's line is not a
/// GPU.
#[test]
fn the_check_uses_the_gpus_nvidia_smi_lists() {
    let rig = Rig::new("dropin-smi-nodes", &[]);
    let ran = rig.run_host(
        &rig.args(&["--nvidia-smi", "/usr/bin/nvidia-smi"]),
        two_gpus,
        no_node,
    );
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: warning: /usr/bin/nvidia-smi -L lists the GPUs {{0, 1}} and {} \
             has the GPU nodes {{}}\n\
             crun-gpu-dropin: --gpu-device: {}/nvidia0 is not a character device; no drop-in \
             written\n",
            rig.dev.display(),
            rig.dev.display()
        )
    );
    assert!(!rig.out.exists());

    let rig = Rig::new("dropin-smi-none", &["nvidia0", "nvidiactl"]);
    let ran = rig.run_with(&["--nvidia-smi", "/usr/bin/nvidia-smi"], no_gpus);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: warning: /usr/bin/nvidia-smi -L lists the GPUs {{}} and {} has \
             the GPU nodes {{0}}\n\
             crun-gpu-dropin: --gpu-device: the host offers no GPU to give a job; no drop-in \
             written\n",
            rig.dev.display()
        )
    );
    assert!(!rig.out.exists());
    assert_eq!(listed_index("GPU 3: NVIDIA A100 (UUID: GPU-x)"), Some(3));
    assert_eq!(listed_index("  MIG 1g.5gb Device 0: (UUID: MIG-x)"), None);
    assert_eq!(listed_index("GPU x: broken"), None);
    assert_eq!(listed_index("GPU 0 no colon"), None);
}

/// A unit whose `ExecStart` quotes a word is refused: the verb splits on white space and would
/// check another command line than the one systemd runs.
#[test]
fn a_quoted_exec_start_is_refused() {
    let rig = Rig::new("dropin-quoted", &["nvidia0"]);
    for quote in ['"', '\''] {
        std::fs::write(
            &rig.unit,
            format!("[Service]\nExecStart={UNIT_LINE} --label {quote}a b{quote}\n"),
        )
        .unwrap();
        let ran = rig.run_with(&[], absent_smi);
        assert_eq!(ran.code, status(1));
        assert_eq!(
            ran.err,
            format!(
                "crun-gpu-dropin: the ExecStart of {} quotes a word, which this verb does not \
                 split as systemd does; no drop-in written\n",
                rig.unit.display()
            )
        );
    }
}

/// The packaged GPU oneshot runs the verb with flags it accepts, for the paths the crun unit and
/// the runbook name.
#[test]
fn the_packaged_oneshot_runs_the_verb_with_flags_it_accepts() {
    let unit = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/js-agent/js-agent-crun-gpu.service"
    ))
    .unwrap();
    let line = exec_start([unit.as_str()]).unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words[1], "crun-gpu-dropin");
    let args = Args::try_parse_from(&words[1..]).unwrap();
    assert_eq!(
        args.out,
        Path::new("/run/systemd/system/js-agent-crun.service.d/zz-gpu.conf")
    );
    assert_eq!(
        args.unit,
        Path::new("/etc/systemd/system/js-agent-crun.service")
    );
    assert_eq!(args.dev_root, Path::new("/dev"));
    assert_eq!(args.driver_root, Path::new("/opt/nvidia-driver"));
    assert_eq!(args.nvidia_smi.as_deref(), Some("/usr/bin/nvidia-smi"));
}

/// A unit whose `ExecStart` lists a GPU flag already is the operator's: no drop-in, exit 0.
#[test]
fn a_unit_with_gpu_flags_by_hand_is_left_to_them() {
    let rig = Rig::new("dropin-by-hand", &["nvidia0"]);
    std::fs::write(
        &rig.unit,
        format!("[Service]\nExecStart={UNIT_LINE} --gpu-device /dev/nvidiactl\n"),
    )
    .unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0));
    assert!(!rig.out.exists());
    assert_eq!(
        ran.out,
        format!(
            "crun-gpu-dropin: the ExecStart of {} lists GPU flags already: no drop-in written\n",
            rig.unit.display()
        )
    );
}

/// Each step that fails writes nothing and exits 1 naming what failed: no unit, a unit without
/// `ExecStart`, a unit line the agent's parser refuses, a driver path that cannot stand in a
/// unit line, a `/dev` that cannot be listed, and a drop-in that cannot be written.
#[test]
fn each_failed_step_exits_1_naming_it() {
    let mut rig = Rig::new("dropin-steps", &["nvidia0"]);
    let unit = rig.unit.display().to_string();

    std::fs::remove_file(&rig.unit).unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: {unit}: No such file or directory (os error 2); no drop-in \
             written\n"
        )
    );

    std::fs::write(&rig.unit, "[Unit]\nDescription=x\n").unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(
        ran.err,
        format!("crun-gpu-dropin: {unit} has no ExecStart= in [Service]; no drop-in written\n")
    );

    std::fs::write(
        &rig.unit,
        "[Service]\nExecStart=/x/js-agent --no-such-flag\n",
    )
    .unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert!(
        ran.err.starts_with(&format!(
            "crun-gpu-dropin: the ExecStart of {unit} with the GPU flags is not an agent \
             command line: error: unexpected argument '--no-such-flag'"
        )),
        "{}",
        ran.err
    );

    std::fs::write(&rig.unit, UNIT).unwrap();
    let driver = std::mem::replace(&mut rig.driver, rig.dir.join("nvidia driver"));
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: \"{}:/usr/local/nvidia/lib64\" cannot stand in an ExecStart= \
             line (white space, a quote, a backslash, % or $); no drop-in written\n",
            rig.driver.join("lib64").display()
        )
    );
    rig.driver = driver;

    let dev = std::mem::replace(&mut rig.dev, rig.dir.join("no-dev"));
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: {}: No such file or directory (os error 2); no drop-in written\n",
            rig.dev.display()
        )
    );
    rig.dev = dev;

    // The drop-in's directory is a file: it cannot be made.
    std::fs::create_dir_all(rig.out.parent().unwrap().parent().unwrap()).unwrap();
    std::fs::write(rig.out.parent().unwrap(), "").unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: {out}: File exists (os error 17); no drop-in written\n\
             crun-gpu-dropin: warning: the earlier drop-in {out} cannot be removed: Not a \
             directory (os error 20)\n",
            out = rig.out.display()
        )
    );
}

/// An earlier drop-in that cannot be removed (a directory is there) is a warning.
#[test]
fn an_earlier_dropin_that_cannot_be_removed_is_a_warning() {
    let rig = Rig::new("dropin-stuck", &[]);
    std::fs::create_dir_all(&rig.out).unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0));
    assert!(
        ran.err.starts_with(&format!(
            "crun-gpu-dropin: warning: the earlier drop-in {} cannot be removed: ",
            rig.out.display()
        )),
        "{}",
        ran.err
    );
}

/// The verb without `--out` is a usage error.
#[test]
fn the_verb_without_out_is_a_usage_error() {
    let rig = Rig::new("dropin-usage", &[]);
    let args: Vec<OsString> = vec!["js-agent".into(), "crun-gpu-dropin".into()];
    let ran = rig.run_host(&args, absent_smi, any_node);
    assert_eq!(ran.code, status(2));
    assert!(ran.err.contains("--out <OUT>"), "{}", ran.err);
}

/// `ExecStart=` as systemd reads it: from `[Service]` only, a line that ends with a backslash
/// goes on, an empty value clears what came before, and the last one wins.
#[test]
fn exec_start_is_read_as_systemd_reads_it() {
    let unit = "[Unit]\nExecStart=/unit\n[Service]\n  ExecStart=/a \\\n  --b c\n";
    assert_eq!(exec_start([unit]).as_deref(), Some("/a    --b c"));
    assert_eq!(
        exec_start([unit, "[Service]\nExecStart=\nExecStart=/d\n"]).as_deref(),
        Some("/d")
    );
    assert_eq!(exec_start([unit, "[Service]\nExecStart=\n"]), None);
    assert_eq!(exec_start(["[Install]\nExecStart=/x\n"]), None);
    // Spaces around `=`, and a backslash on the last line.
    assert_eq!(
        exec_start(["[Service]\nExecStart = /e \\"]).as_deref(),
        Some("/e")
    );
    assert_eq!(exec_start(["[Service]\nExecStartPre=/p\n"]), None);
    let text = dropin("/x --y");
    assert!(
        text.ends_with("[Service]\nExecStart=\nExecStart=/x --y\n"),
        "{text}"
    );
}

/// The unit hostkeeper links names only `--flags-file` and `--token-file`: the server and the
/// pool are in the operator's flags file. The checks read the file as the agent does, and the
/// drop-in keeps `--flags-file` as the unit has it, so the agent reads the file at its start.
#[test]
fn the_dropin_extends_the_hostkeeper_unit_whose_server_is_in_the_flags_file() {
    let mut rig = Rig::new("dropin-hostkeeper", &["nvidia0", "nvidiactl"]);
    rig.flags = Some("--server=https://js.example.com:8443\n--pool=farm\n");
    let unit = std::fs::read_to_string(HOSTKEEPER_UNIT).unwrap();
    std::fs::write(&rig.unit, &unit).unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    let own = exec_start([unit.as_str()]).unwrap();
    assert!(own.contains(" --flags-file /etc/js-agent/flags "), "{own}");
    assert!(!own.contains("--server"), "{own}");
    let line = rig.written_line(&unit);
    let mut want = words(&own);
    want.extend(words(
        "--gpu-device /dev/nvidia{index} --gpu-device /dev/nvidiactl",
    ));
    want.extend(rig.driver_flags());
    assert_eq!(words(&line), want);
}

/// A flags file the agent would refuse is refused here too, naming the unit and the file, and
/// no drop-in is left: the agent's own start then says the same.
#[test]
fn a_flags_file_the_agent_refuses_writes_no_dropin() {
    let mut rig = Rig::new("dropin-flags-refused", &["nvidia0"]);
    rig.flags = Some("server=https://js.example.com:8443\n");
    std::fs::write(
        &rig.unit,
        "[Service]\nExecStart=/a/js-agent --flags-file /etc/js-agent/flags\n",
    )
    .unwrap();
    std::fs::create_dir_all(rig.out.parent().unwrap()).unwrap();
    std::fs::write(&rig.out, "stale").unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(1));
    assert_eq!(
        ran.err,
        format!(
            "crun-gpu-dropin: the ExecStart of {}: --flags-file /etc/js-agent/flags: line 1: \
             not a flag (every line must start with --); no drop-in written\n",
            rig.unit.display()
        )
    );
    assert!(!rig.out.exists());
}

/// GPU flags the operator lists in the flags file win as those in the unit do: no drop-in.
#[test]
fn gpu_flags_in_the_flags_file_win_over_the_dropin() {
    let mut rig = Rig::new("dropin-flags-gpu", &["nvidia0"]);
    rig.flags = Some("--server=https://s.example\n--pool=p\n--gpu-device=/dev/nvidia0\n");
    std::fs::write(
        &rig.unit,
        "[Service]\nExecStart=/a/js-agent --flags-file /etc/js-agent/flags\n",
    )
    .unwrap();
    let ran = rig.run_with(&[], absent_smi);
    assert_eq!(ran.code, status(0), "{}", ran.err);
    assert_eq!(
        ran.out,
        format!(
            "crun-gpu-dropin: the ExecStart of {} lists GPU flags already: no drop-in written\n",
            rig.unit.display()
        )
    );
    assert!(!rig.out.exists());
}
