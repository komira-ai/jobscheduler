//! The unit's recovery step, run by systemd: a unit with the packaged unit's delegation,
//! `KillMode=` and `ExecStopPost=` (this build's `js-agent crun-start-refused`), whose agent
//! leaves a job behind at its stop as a hand-over does (a container's stand-in in a lease cgroup
//! under `<unit>/actions`, with `memory` and `pids` enabled for it as the agent's delegate step
//! enables them, and a supervisor's stand-in in the agent's leaf). systemd runs the step in
//! `<unit>/.control` after the stop; the step finds the unit's cgroup from its own, ends both
//! stand-ins through the kernel's `cgroup.kill`, and the next start of the unit starts the agent.
//!
//! What a node has that the runner does not is stood in by the step's command line, and only
//! that: the runner's systemd is 255, which places a service's first process in its subgroup and
//! so never refuses this start (systemd 258 and newer do, `crates/js-agent/src/systemd.rs`), and
//! never reports the result `resources` for it. So the unit's `ExecStopPost=` passes
//! `--result resources`, where the packaged unit passes `${SERVICE_RESULT}` (which systemd sets
//! to `resources` after a start it refused, and runs `ExecStopPost=` the same way, in the same
//! `.control` leaf), and `--systemctl` names a program that prints `systemd 259`. The step is
//! then run by systemd after the stop that left the job, which is the state a refused start
//! finds.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::unit::{PROBE_DIR, alive, end_all, fifo, heard, idle, packaged, run, say};

/// The cgroup v2 mount.
const MOUNT: &str = "/sys/fs/cgroup";

/// The lease cgroup the probe makes.
const LEASE: &str = "lease-refused";

/// Where systemd reads the test's unit file from (gone at a reboot).
const UNITS: &str = "/run/systemd/system";

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

/// The probe of the refused-start test, started by it as its unit's agent. Its name holds that
/// of the unit test's probe, so the workflow's `--skip probe_of_the_unit` skips it too; run
/// without its unit it fails, saying so. On its first start it lays out the unit as the agent
/// does and leaves a job in it; on the next it says it started.
#[test]
#[ignore = "the probe of the refused-start test: started by that test in its own unit"]
fn probe_of_the_unit_with_a_refused_start() {
    let dir = PathBuf::from(
        std::env::var_os(PROBE_DIR)
            .unwrap_or_else(|| panic!("{PROBE_DIR} is not set: this runs only as a unit's probe")),
    );
    let pids = dir.join("pids");
    if pids.exists() {
        say(&dir.join("second"), "started");
    } else {
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
        fs::write(&pids, format!("{container} {supervisor}")).expect("write the pids");
        say(&dir.join("first"), "up");
    }
    loop {
        std::thread::park();
    }
}

/// The lines the step wrote to the unit's output file `log`, with its prefix.
fn step_lines(log: &Path) -> String {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("crun-start-refused: "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Proves, with systemd running the unit's `ExecStopPost=` step as the packaged unit has it,
/// that the jobs a stop leaves under controllers enabled for their lease cgroup are ended by
/// the step, and that the next start of the unit starts its agent: the stop leaves both
/// stand-ins running; the step, which systemd runs after it, names the lease; the kernel has
/// ended both; and the agent of the next start says it started. Catches a step that systemd does
/// not run where the packaged unit puts it, one that finds the wrong cgroup from its own leaf, a
/// kill written where the kernel does not act on it (or the supervisors' leaf left out), and a
/// step that says it ended jobs that still run.
#[test]
#[ignore = "real systemd unit with DelegateSubgroup=supervisor: run by the crun-runtime workflow"]
fn systemd_runs_the_recovery_step_after_the_stop_and_the_next_start_brings_the_agent_up() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unit-probe-refused-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make the probe's directory");
    fifo(&dir.join("first"));
    fifo(&dir.join("second"));
    let systemctl = dir.join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\necho 'systemd 259 (259.5)'\n").expect("write");
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).expect("chmod");
    let unit = format!("js-unit-probe-refused-{}.service", std::process::id());
    let log = dir.join("unit.log");
    let exe = std::env::current_exe().expect("this test binary");
    let file = Path::new(UNITS).join(&unit);
    fs::write(
        &file,
        format!(
            "[Service]\nType=simple\nDelegate={}\nDelegateSubgroup={}\nKillMode={}\n\
             TimeoutStopSec=10\nEnvironment={PROBE_DIR}={}\nStandardOutput=append:{}\n\
             ExecStart={} --exact refused::probe_of_the_unit_with_a_refused_start \
             --include-ignored --test-threads=1\n\
             ExecStopPost={} crun-start-refused --result resources --systemctl {}\n",
            packaged("Delegate"),
            packaged("DelegateSubgroup"),
            packaged("KillMode"),
            dir.display(),
            log.display(),
            exe.display(),
            env!("CARGO_BIN_EXE_js-agent"),
            systemctl.display(),
        ),
    )
    .expect("write the unit");
    run("systemctl", &["daemon-reload"]);
    run("systemctl", &["start", &unit]);
    assert_eq!(heard(&dir.join("first")), "up");
    let text = fs::read_to_string(dir.join("pids")).expect("read the pids");
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|pid| pid.parse().expect("a pid"))
        .collect();

    // The stop leaves the job (KillMode=process), and systemd runs the step after it.
    run("systemctl", &["stop", &unit]);
    let left: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
    let said = step_lines(&log);

    run("systemctl", &["start", &unit]);
    let started = heard(&dir.join("second"));
    run("systemctl", &["stop", &unit]);
    end_all(&pids);
    let _ = fs::remove_file(&file);
    run("systemctl", &["daemon-reload"]);
    let _ = std::process::Command::new("systemctl")
        .args(["reset-failed", &unit])
        .status();

    assert!(
        said.contains(LEASE) && said.contains("systemd 259"),
        "the step said: {said:?}"
    );
    assert!(
        left.is_empty(),
        "still running after the step: {left:?}; {said}"
    );
    assert_eq!(started, "started");
}
