//! `js-agent crun-start-refused` on a real delegated unit: the jobs a stop of the unit leaves (a
//! container's stand-in in a lease cgroup under `<unit>/actions`, with `memory` and `pids`
//! enabled for it as the agent's delegate step enables them, and a supervisor's stand-in in the
//! agent's leaf) are ended by the kernel's `cgroup.kill` as the unit's `ExecStopPost=` step ends
//! them after a start systemd refused. With nothing left in it, systemd then removes the unit's
//! cgroup, so the next start makes a new one with no controller enabled, which systemd 258 and
//! newer can place the agent's first process in.
//!
//! The runner's systemd is 255, which places a service's first process in its subgroup and so
//! never refuses this start; systemd 258 and newer do (`crates/js-agent/src/systemd.rs`). The
//! test therefore runs the verb on the real cgroups of the stopped unit with the version stood in
//! (`systemd 259`) and this process's own cgroup line pointing at the unit's `.control` leaf, as
//! systemd runs `ExecStopPost=` there. Everything else is the verb's own: the files it reads and
//! writes are the kernel's.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::start_refused::{Host, VERB, run as refused};

use super::unit::{PROBE_DIR, alive, end_all, fifo, heard, idle, packaged, run, say};

/// The cgroup v2 mount.
const MOUNT: &str = "/sys/fs/cgroup";

/// The lease cgroup the probe makes.
const LEASE: &str = "lease-refused";

/// The probe's unit's cgroup directory: the parent of its leaf.
fn own_unit() -> PathBuf {
    let own = fs::read_to_string("/proc/self/cgroup").expect("read /proc/self/cgroup");
    let leaf = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("cgroup v2");
    Path::new(MOUNT)
        .join(leaf.trim_start_matches('/'))
        .parent()
        .expect("the leaf has a parent")
        .to_owned()
}

/// The probe of the refused-start test, started by it in its own unit. Its name holds that of
/// the unit test's probe, so the workflow's `--skip probe_of_the_unit` skips it too; run without
/// its unit it fails, saying so. It lays out the unit as the agent does and leaves a job in it.
#[test]
#[ignore = "the probe of the refused-start test: started by that test in its own unit"]
fn probe_of_the_unit_with_a_refused_start() {
    let dir = PathBuf::from(
        std::env::var_os(PROBE_DIR)
            .unwrap_or_else(|| panic!("{PROBE_DIR} is not set: this runs only as a unit's probe")),
    );
    let unit = own_unit();
    let actions = unit.join("actions");
    fs::write(unit.join("cgroup.subtree_control"), "+memory +pids").expect("enable");
    fs::create_dir_all(actions.join(LEASE)).expect("make the lease cgroup");
    fs::write(actions.join("cgroup.subtree_control"), "+memory +pids").expect("enable");
    let container = idle();
    fs::write(
        actions.join(LEASE).join("cgroup.procs"),
        container.to_string(),
    )
    .expect("move into the lease");
    let supervisor = idle();
    fs::write(dir.join("pids"), format!("{container} {supervisor}")).expect("write the pids");
    say(&dir.join("first"), "up");
    loop {
        std::thread::park();
    }
}

/// Proves on a real unit with the packaged unit's delegation and `KillMode=` that the jobs a
/// stop leaves, under controllers enabled for their lease cgroup, are ended by the verb the unit
/// runs as its `ExecStopPost=` after a refused start: it names the lease, the kernel ends both
/// stand-ins (supervisor and container), the lease cgroups hold no process, and systemd removes
/// the unit's cgroup, which nothing holds any more. Catches a kill written where the kernel does
/// not act on it (the wrong cgroup, or the supervisors' leaf left out), and a verb that reports
/// success over a cgroup still populated.
#[test]
#[ignore = "real systemd unit with DelegateSubgroup=supervisor: run by the crun-runtime workflow"]
fn a_refused_start_ends_the_jobs_a_stop_left_and_systemd_frees_the_unit_cgroup() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unit-probe-refused-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make the probe's directory");
    fifo(&dir.join("first"));
    let unit = format!("js-unit-probe-refused-{}", std::process::id());
    let exe = std::env::current_exe().expect("this test binary");
    let set = |key: &str, value: &str| format!("--property={key}={value}");
    run(
        "systemd-run",
        &[
            "--quiet",
            &format!("--unit={unit}"),
            &set("Delegate", &packaged("Delegate")),
            &set("DelegateSubgroup", &packaged("DelegateSubgroup")),
            &set("KillMode", &packaged("KillMode")),
            &set("TimeoutStopSec", "10"),
            &format!("--setenv={PROBE_DIR}={}", dir.display()),
            "--",
            exe.to_str().expect("a path in UTF-8"),
            "--exact",
            "refused::probe_of_the_unit_with_a_refused_start",
            "--include-ignored",
            "--test-threads=1",
        ],
    );
    assert_eq!(heard(&dir.join("first")), "up");
    run("systemctl", &["stop", &unit]);
    let text = fs::read_to_string(dir.join("pids")).expect("read the pids");
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|pid| pid.parse().expect("a pid"))
        .collect();
    assert!(pids.iter().all(|pid| alive(*pid)), "the stop left the job");

    let cgroup = Path::new("/system.slice").join(format!("{unit}.service"));
    let own = dir.join("self-cgroup");
    fs::write(&own, format!("0::{}/.control\n", cgroup.display())).expect("write");
    let host = Host {
        systemctl: "systemctl",
        version: &|_| Ok("systemd 259 (259.5)\n".to_owned()),
        self_cgroup: &own,
        pause: &|_| std::thread::sleep(Duration::from_millis(100)),
        ..Host::system()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = ["js-agent", VERB, "--result", "resources"].map(std::ffi::OsString::from);
    let code = refused(&args, &mut out, &mut err, &host);
    let (out, err) = (
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    );
    let left: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
    end_all(&pids);
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::SUCCESS),
        "{out}{err}"
    );
    assert!(out.contains(LEASE) && out.contains("systemd 259"), "{out}");
    assert!(left.is_empty(), "still running: {left:?}; {out}");
    // systemd learns from the kernel's event that the cgroup emptied, then unloads the unit and
    // removes its cgroup: a bounded count of looks, each after a short pause.
    let tree = Path::new(MOUNT).join(cgroup.strip_prefix("/").expect("absolute"));
    let mut looks = 0;
    while tree.exists() && looks < 100 {
        std::thread::sleep(Duration::from_millis(100));
        looks += 1;
    }
    let _ = std::process::Command::new("systemctl")
        .args(["reset-failed", &unit])
        .status();
    assert!(!tree.exists(), "systemd kept the cgroup {}", tree.display());
}
