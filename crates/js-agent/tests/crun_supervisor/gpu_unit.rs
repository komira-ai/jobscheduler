//! The packaged GPU oneshot and the crun unit's dependency on it, in real systemd: starting the
//! agent's unit runs `js-agent crun-gpu-dropin` first, reloads systemd, and the agent's process
//! is started with the flags the drop-in wrote; on a host whose GPU nodes are gone the next start
//! gets the unit's own command line again.
//!
//! Both units are made from the packaged files with only their paths and names changed (each
//! change must find its text, so a packaged unit that drifts fails here): the oneshot runs this
//! build's `js-agent` over a `/dev` of its own (character devices made with `mknod`) and a driver
//! directory of its own, and the agent's unit keeps the packaged `Wants=`/`After=` on the oneshot
//! and the packaged `ExecStart` flags, run by `/usr/bin/echo` into a file, which is what the
//! test reads. Needs root and systemd; the crun-runtime workflow runs it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PACKAGED_UNIT: &str = "../../packaging/js-agent/js-agent-crun.service";
const PACKAGED_GPU_UNIT: &str = "../../packaging/js-agent/js-agent-crun-gpu.service";
const UNIT_DIR: &str = "/run/systemd/system";

fn packaged(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The value of the one `key=` line of `text`.
fn value(text: &str, key: &str) -> String {
    let found: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{key}=")))
        .collect();
    assert_eq!(found.len(), 1, "{key}: {found:?}");
    found[0].trim().to_owned()
}

/// `text` with `from` replaced by `to`, where `from` is there exactly once.
fn swap(text: &str, from: &str, to: &str) -> String {
    assert_eq!(
        text.matches(from).count(),
        1,
        "the packaged unit has no single {from:?}"
    );
    text.replace(from, to)
}

fn systemctl(args: &[&str]) {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run systemctl: {e}"));
    assert!(
        out.status.success(),
        "systemctl {args:?}: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Makes a character device at `path` (the numbers of `/dev/null`).
fn node(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::CharacterDevice,
        rustix::fs::Mode::from_raw_mode(0o666),
        rustix::fs::makedev(1, 3),
    )
    .unwrap_or_else(|e| panic!("mknod {}: {e}", path.display()));
}

/// Removes the units, the drop-in and the probe's directory, and reloads systemd.
struct Units {
    files: Vec<PathBuf>,
    dropins: PathBuf,
    names: Vec<String>,
    dir: PathBuf,
}

impl Drop for Units {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new("systemctl").args(["stop", name]).status();
            let _ = Command::new("systemctl")
                .args(["reset-failed", name])
                .status();
        }
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        let _ = fs::remove_dir_all(&self.dropins);
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Proves, in real systemd, the packaged start sequence: a start of the agent's unit pulls in the
/// GPU oneshot, which writes the drop-in for the host's nodes and reloads systemd before the
/// agent's process starts, so that process gets the packaged `ExecStart` followed by the
/// generated `--gpu-*` flags. After the GPU nodes are gone, the next start removes the drop-in and
/// the process gets the packaged `ExecStart` alone. Catches a drop-in systemd does not read for the
/// unit, an order in which the agent starts before the reload, and a stale drop-in left behind.
#[test]
#[ignore = "real systemd units, as root: run by the crun-runtime workflow"]
fn the_agent_starts_with_the_flags_the_gpu_oneshot_wrote_before_it() {
    let pid = std::process::id();
    let agent = format!("js-gpu-probe-agent-{pid}.service");
    let gpu = format!("js-gpu-probe-oneshot-{pid}.service");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("gpu-unit-probe-{pid}"));
    let _ = fs::remove_dir_all(&dir);
    let dev = dir.join("dev");
    let driver = dir.join("nvidia-driver");
    for sub in [&dev, &driver.join("lib64"), &driver.join("bin")] {
        fs::create_dir_all(sub).expect("make the probe's directories");
    }
    for name in ["nvidia0", "nvidia1", "nvidiactl", "nvidia-uvm"] {
        node(&dev.join(name));
    }
    let argv = dir.join("argv");
    let agent_path = Path::new(UNIT_DIR).join(&agent);
    let gpu_path = Path::new(UNIT_DIR).join(&gpu);
    let dropins = Path::new(UNIT_DIR).join(format!("{agent}.d"));
    let _units = Units {
        files: vec![agent_path.clone(), gpu_path.clone()],
        dropins: dropins.clone(),
        names: vec![agent.clone(), gpu.clone()],
        dir: dir.clone(),
    };

    // The agent's unit: the packaged dependency on the oneshot and the packaged flags.
    let crun = packaged(PACKAGED_UNIT);
    let packaged_flags: Vec<String> = value(&crun, "ExecStart")
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect();
    let on_oneshot = |key: &str| {
        let words = value(&crun, key);
        assert!(
            words
                .split_whitespace()
                .any(|w| w == "js-agent-crun-gpu.service"),
            "the packaged unit's {key}= does not name the oneshot: {words}"
        );
        format!("{key}={gpu}\n")
    };
    let agent_text = format!(
        "[Unit]\n{}{}[Service]\nType=oneshot\nStandardOutput=truncate:{}\nExecStart=/usr/bin/echo {}\n",
        on_oneshot("Wants"),
        on_oneshot("After"),
        argv.display(),
        packaged_flags.join(" ")
    );
    fs::write(&agent_path, agent_text).expect("write the agent's unit");

    // The oneshot: the packaged file, with this build's program and the probe's paths.
    let mut oneshot = packaged(PACKAGED_GPU_UNIT);
    for (from, to) in [
        (
            "/opt/js-agent/link/current/js-agent ".to_owned(),
            format!("{} ", env!("CARGO_BIN_EXE_js-agent")),
        ),
        (
            "--out /run/systemd/system/js-agent-crun.service.d/zz-gpu.conf".to_owned(),
            format!("--out {}", dropins.join("zz-gpu.conf").display()),
        ),
        (
            "--unit /etc/systemd/system/js-agent-crun.service".to_owned(),
            format!("--unit {}", agent_path.display()),
        ),
        (
            "Before=js-agent-crun.service".to_owned(),
            format!("Before={agent}"),
        ),
        (
            "--dev-root /dev ".to_owned(),
            format!("--dev-root {} ", dev.display()),
        ),
        (
            "--driver-root /opt/nvidia-driver ".to_owned(),
            format!("--driver-root {} ", driver.display()),
        ),
        (
            "--nvidia-smi /usr/bin/nvidia-smi".to_owned(),
            format!("--nvidia-smi {}", dir.join("no-nvidia-smi").display()),
        ),
    ] {
        oneshot = swap(&oneshot, &from, &to);
    }
    fs::write(&gpu_path, oneshot).expect("write the oneshot");
    systemctl(&["daemon-reload"]);

    systemctl(&["start", &agent]);
    let ran: Vec<String> = fs::read_to_string(&argv)
        .expect("the agent's process ran")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut want = packaged_flags.clone();
    for word in [
        "--gpu-device",
        "/dev/nvidia{index}",
        "--gpu-device",
        "/dev/nvidia-uvm",
        "--gpu-device",
        "/dev/nvidiactl",
        "--gpu-mount",
        &format!("{}/lib64:/usr/local/nvidia/lib64", driver.display()),
        "--gpu-mount",
        &format!("{}/bin:/usr/local/nvidia/bin", driver.display()),
        "--gpu-env",
        "LD_LIBRARY_PATH=/usr/local/nvidia/lib64",
        "--gpu-env",
        "PATH=/usr/local/nvidia/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ] {
        want.push(word.to_owned());
    }
    assert_eq!(
        ran, want,
        "the agent did not start with the generated flags"
    );

    // The GPUs are gone: the next start removes the drop-in before the agent starts.
    for name in ["nvidia0", "nvidia1"] {
        fs::remove_file(dev.join(name)).expect("remove a GPU node");
    }
    systemctl(&["start", &agent]);
    let ran: Vec<String> = fs::read_to_string(&argv)
        .expect("the agent's process ran again")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert_eq!(ran, packaged_flags, "a stale drop-in reached the agent");
    assert!(!dropins.join("zz-gpu.conf").exists());
}
