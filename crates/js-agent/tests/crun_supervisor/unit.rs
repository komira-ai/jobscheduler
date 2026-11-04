//! The packaged unit's kill settings, in a real systemd unit: a crash of the agent leaves what
//! a crun job is made of (its supervisor in the agent's leaf, its container in a lease cgroup
//! under `<unit>/actions`) alive, and `Restart=` brings the agent back to find it.
//!
//! The agent here is a probe: this test binary again, run in a transient unit as
//! `--exact unit::probe_of_the_unit` with the packaged unit's `KillMode=` and `Restart=` and its
//! delegation. On its first start the probe makes a lease cgroup, starts a stand-in container
//! (a process it moves into that cgroup) and a stand-in supervisor (a process of its own group
//! left in the leaf), and says so through a named pipe. The test kills the probe's main process
//! with `SIGKILL`; the probe that systemd starts again reads whether both processes still run and
//! says that through a second pipe. No clock is read: each step waits for the other side's word.
//! What `crun` itself does after the restart (the runtime adopts the container) is proved by
//! `tests/crun_runtime/adopt.rs`.

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The variable that makes the probe act; set only on the probe's unit.
const PROBE_DIR: &str = "JS_UNIT_PROBE_DIR";

/// The cgroup v2 mount.
const MOUNT: &str = "/sys/fs/cgroup";

/// The packaged unit's value of `key` in its `[Service]` section.
fn packaged(key: &str) -> String {
    let unit = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/js-agent/js-agent-crun.service");
    let text = fs::read_to_string(unit).expect("read the packaged unit");
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("the packaged unit sets no {key}"))
        .trim()
        .to_owned()
}

/// Runs `program` with `args` and fails the test, with what it said, when it fails.
fn run(program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Makes the named pipe `path`.
fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap_or_else(|e| panic!("make the pipe {}: {e}", path.display()));
}

/// The word written to the pipe `path`, waiting until the other side opens and closes it.
fn heard(path: &Path) -> String {
    let mut word = String::new();
    fs::File::open(path)
        .and_then(|mut pipe| pipe.read_to_string(&mut word))
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    word.trim().to_owned()
}

/// Writes `word` to the pipe `path`, waiting until the other side opens it.
fn say(path: &Path, word: &str) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut pipe| pipe.write_all(word.as_bytes()))
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Whether the process `pid` runs: it is there and not a zombie.
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

/// The probe's own unit's cgroup directory: the parent of its leaf.
fn own_unit() -> PathBuf {
    let own = fs::read_to_string("/proc/self/cgroup").expect("read /proc/self/cgroup");
    let leaf = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("cgroup v2");
    assert!(leaf.ends_with("/supervisor"), "not in a leaf: {leaf}");
    Path::new(MOUNT)
        .join(leaf.trim_start_matches('/'))
        .parent()
        .expect("the leaf has a parent")
        .to_owned()
}

/// A process that runs until it is killed, in a process group of its own, as a supervisor and
/// a container's init are.
fn idle() -> u32 {
    let mut command = Command::new("sleep");
    command
        .arg("infinity")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // Left running on purpose: the process stands for a job that outlives its agent, and the
    // probe kills it by its pid when the case is over.
    #[allow(clippy::zombie_processes)]
    let child = command.spawn().expect("start sleep");
    child.id()
}

/// The probe: the agent of the unit `the_unit_leaves_leases_alive_when_the_agent_is_killed`
/// starts. Not a test of its own: it acts only in that unit, and the workflow skips it
/// elsewhere (`--skip probe_of_the_unit`); run without its unit it fails, saying so.
#[test]
#[ignore = "the probe of the unit test: started by that test in its own unit"]
fn probe_of_the_unit() {
    let dir = PathBuf::from(
        std::env::var_os(PROBE_DIR)
            .unwrap_or_else(|| panic!("{PROBE_DIR} is not set: this runs only as a unit's probe")),
    );
    let pids = dir.join("pids");
    let unit = own_unit();
    let lease = unit.join("actions").join("lease-probe");
    if !pids.exists() {
        // The first start: a lease cgroup with a container's stand-in in it, and a
        // supervisor's stand-in left in the leaf.
        fs::create_dir_all(&lease).expect("make the lease cgroup");
        let container = idle();
        fs::write(lease.join("cgroup.procs"), container.to_string()).expect("move into the lease");
        let supervisor = idle();
        fs::write(&pids, format!("{container} {supervisor}")).expect("write the pids");
        say(&dir.join("first"), "up");
    } else {
        // Started again after the crash: what still runs of the job.
        let text = fs::read_to_string(&pids).expect("read the pids");
        let pids: Vec<u32> = text
            .split_whitespace()
            .map(|pid| pid.parse().expect("a pid"))
            .collect();
        let word = if pids.iter().all(|pid| alive(*pid)) {
            "alive"
        } else {
            "gone"
        };
        // Nothing of the probe is left for the next case.
        let _ = fs::write(lease.join("cgroup.kill"), "1");
        for pid in pids.iter().filter(|pid| alive(**pid)) {
            let pid = rustix::process::Pid::from_raw(i32::try_from(*pid).expect("a pid"));
            if let Some(pid) = pid {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        say(&dir.join("second"), word);
    }
    // The unit's main process stays until it is killed or the unit is stopped.
    loop {
        std::thread::park();
    }
}

/// What the restarted probe found of the job after its first main process was killed, in a
/// unit with the packaged unit's delegation and `Restart=`, and `kill_mode` as its `KillMode=`.
fn after_a_crash(kill_mode: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unit-probe-{kill_mode}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make the probe's directory");
    fifo(&dir.join("first"));
    fifo(&dir.join("second"));
    let unit = format!("js-unit-probe-{kill_mode}-{}", std::process::id());
    let exe = std::env::current_exe().expect("this test binary");
    let set = |key: &str, value: &str| format!("--property={key}={value}");
    let restart = packaged("Restart");
    run(
        "systemd-run",
        &[
            "--quiet",
            &format!("--unit={unit}"),
            &set("Delegate", &packaged("Delegate")),
            &set("DelegateSubgroup", &packaged("DelegateSubgroup")),
            &set("KillMode", kill_mode),
            &set("Restart", &restart),
            // Not the packaged settings, which are not what is tested: a quick restart, and a
            // stop that does not wait the packaged drain's bound before its SIGKILL.
            &set("RestartSec", "1"),
            &set("TimeoutStopSec", "10"),
            &format!("--setenv={PROBE_DIR}={}", dir.display()),
            "--",
            exe.to_str().expect("a path in UTF-8"),
            "--exact",
            "unit::probe_of_the_unit",
            "--include-ignored",
            "--test-threads=1",
        ],
    );
    assert_eq!(heard(&dir.join("first")), "up");
    run(
        "systemctl",
        &["kill", "--kill-whom=main", "--signal=SIGKILL", &unit],
    );
    let found = heard(&dir.join("second"));
    run("systemctl", &["stop", &unit]);
    let _ = Command::new("systemctl")
        .args(["reset-failed", &unit])
        .status();
    found
}

/// Proves, in a real systemd unit with the packaged unit's `KillMode=`, `Restart=` and
/// delegation, that a crash of the agent's main process (`SIGKILL`) leaves a crun job alive: the
/// process standing for its container (in `<unit>/actions/<lease>`) and the one standing for its
/// supervisor (in the agent's leaf, a process group of its own) still run when `Restart=` has
/// brought the agent back. The same probe under `KillMode=mixed`, which the unit set before,
/// finds them gone: the test fails on that setting.
#[test]
#[ignore = "real systemd unit with DelegateSubgroup=supervisor: run by the crun-runtime workflow"]
fn the_unit_leaves_leases_alive_when_the_agent_is_killed() {
    assert_eq!(packaged("KillMode"), "process");
    assert_eq!(after_a_crash(&packaged("KillMode")), "alive");
    assert_eq!(after_a_crash("mixed"), "gone");
}
