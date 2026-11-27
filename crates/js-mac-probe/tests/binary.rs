//! The binary's child modes, started as the probe starts them inside a profile.

use std::process::Command;

use js_testkit::dir::TestDir;

fn probe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_js-mac-probe"))
}

#[test]
fn the_write_child_creates_a_file_and_exits_zero() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-write");
    let path = scratch.join("file");
    let out = probe()
        .arg("child")
        .arg("write")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"probe\n");
}

#[test]
fn a_refused_child_exits_one_with_the_reason_on_stderr() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("no-such-dir")
        .join("f");
    let out = probe()
        .arg("child")
        .arg("write")
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("errno=2 "), "{stderr}");
}

/// What a child mode did: its exit code or the signal that ended it, and its output.
fn child(args: &[&str]) -> std::process::Output {
    probe().arg("child").args(args).output().unwrap()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Writing past `RLIMIT_FSIZE` ends the process with `SIGXFSZ`, and the file stops at the
/// limit (Linux writes up to the limit, then signals). Catches a limit that is not set, or
/// set on the wrong resource, either of which lets the write finish and exit 0.
#[test]
fn write_bytes_past_its_file_limit_is_ended_by_sigxfsz() {
    use std::os::unix::process::ExitStatusExt as _;
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-fsize");
    let path = scratch.join("file");
    let out = child(&[
        "write-bytes",
        "--limit",
        "4096",
        "--bytes",
        "65536",
        path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.signal(), Some(libc::SIGXFSZ), "{out:?}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
}

/// Under a limit larger than the write, the whole file is written and the mode says so.
/// The limit leaves room for the coverage profile the instrumented binary writes at exit.
#[test]
fn write_bytes_within_its_file_limit_writes_every_byte() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-fsize-room");
    let path = scratch.join("file");
    let out = child(&[
        "write-bytes",
        "--limit",
        "67108864",
        "--bytes",
        "5000",
        path.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "wrote=5000\n");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 5000);
}

/// A file that cannot be created ends the mode with exit 1 and `ENOENT` before it writes.
#[test]
fn write_bytes_into_a_missing_directory_is_refused() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-fsize-missing");
    let path = scratch.join("no-such-dir").join("file");
    let out = child(&[
        "write-bytes",
        "--limit",
        "67108864",
        "--bytes",
        "1",
        path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("errno=2 "), "{out:?}");
}

/// Linux enforces `RLIMIT_AS` (macOS does not, which `limits/as-not-enforced` checks): an
/// allocation larger than the limit is refused and the mode exits 1 naming the size. A
/// smaller one under the same limit succeeds. Catches a mode that sets no limit (the large
/// allocation then succeeds) and one that reports an allocation it did not make.
#[test]
fn allocate_is_held_to_its_address_space_limit_on_linux() {
    let gib = "1073741824";
    let out = child(&["allocate", "--limit", gib, "--bytes", "4294967296"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).starts_with("errno=0 cannot allocate 4294967296 bytes: "),
        "{out:?}"
    );
    let out = child(&["allocate", "--limit", gib, "--bytes", "1048576"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "allocated=1048576\n");
}

/// Past its soft `RLIMIT_CPU` the process is ended with `SIGXCPU`, long before its rounds
/// of work are done. Catches a limit that is not set (the rounds then end in exit 0) and
/// one set at the hard limit alone (Linux then sends `SIGKILL`).
#[test]
fn burn_past_its_cpu_limit_is_ended_by_sigxcpu() {
    use std::os::unix::process::ExitStatusExt as _;
    let out = child(&["burn", "--seconds", "1", "--rounds", "100000"]);
    assert_eq!(out.status.signal(), Some(libc::SIGXCPU), "{out:?}");
}

/// Within its CPU limit the mode does its rounds and exits 0.
#[test]
fn burn_within_its_cpu_limit_does_its_rounds() {
    let out = child(&["burn", "--seconds", "3600", "--rounds", "1"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "burned=1\n");
}

/// `RLIMIT_NPROC` counts every process of the uid: at 2, the child (one process, no
/// children) cannot start another, because this test's process has the same uid; without
/// the limit the same start succeeds. The test runs unprivileged, as CI runs it (root is
/// exempt from the limit on Linux and macOS). Catches a limit that is not set and a mode
/// that reports a start it did not make.
#[test]
fn spawn_is_refused_once_the_uid_is_at_its_process_limit() {
    let exe = env!("CARGO_BIN_EXE_js-mac-probe");
    let target = [exe, "child", "env", "HOME"];
    let out = child(&[&["spawn"][..], &target[..]].concat());
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "spawned exit status: 0\n"
    );
    let out = child(&[&["spawn", "--max-processes", "2"][..], &target[..]].concat());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).starts_with(&format!("errno={} ", libc::EAGAIN)),
        "{out:?}"
    );
}

/// A program that cannot be started ends the mode with exit 1 and `ENOENT`, not a report
/// of a start.
#[test]
fn spawn_of_a_missing_program_is_refused_with_enoent() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-spawn-missing");
    let missing = scratch.join("no-such-program");
    let out = child(&["spawn", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).starts_with("errno=2 "), "{out:?}");
}

#[test]
fn the_launchd_job_child_starts_hold_writes_both_pids_and_ends_with_it() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-launchd-job");
    let ready = scratch.join("ready");
    let child = probe()
        .args(["child", "launchd-job", "--secs", "0", "--tag", "t", "--out"])
        .arg(&ready)
        .spawn()
        .unwrap();
    let job = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&ready).unwrap();
    let (written_job, hold) = js_mac_probe::launchd::ready(&text).unwrap();
    assert_eq!(written_job, job);
    assert_ne!(hold, job);
}

#[test]
fn the_hold_child_prints_its_tag() {
    let out = probe()
        .args(["child", "hold", "--secs", "0", "--tag", "js-mac-probe.7"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"js-mac-probe.7\n");
}
