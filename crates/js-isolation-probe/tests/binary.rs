//! The program itself, and the real tools it runs, driven with this crate's own binary so no
//! test runs a tool of the host.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use js_isolation_probe::tools::{System, Tools as _};

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_js-isolation-probe"))
}

fn tools(limit: Duration) -> System {
    System { limit, exe: exe() }
}

/// The most looks [`reaped`] takes at a killed process.
const LOOKS: u32 = 50_000_000;

/// Whether the process `pid`, killed, is reaped within [`LOOKS`] looks (not a time): `linger`'s
/// thread takes its status, after which the pid names no process and a signal to it fails with
/// `ESRCH` (a killed process not yet reaped still takes one).
fn reaped(tools: &System, pid: i32) -> bool {
    (0..LOOKS).any(|_| {
        let gone = tools
            .kill(pid)
            .is_err_and(|err| err.raw_os_error() == Some(libc::ESRCH));
        if !gone {
            std::thread::yield_now();
        }
        gone
    })
}

#[test]
fn linger_with_no_time_ends_at_once() {
    let out = Command::new(exe())
        .args(["linger", "--token", "t1", "--secs", "0"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "linger prints nothing");
}

#[test]
fn hold_waits_in_its_own_session_and_ends_well() {
    let out = Command::new(exe())
        .args(["hold", "--secs", "0"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "hold prints nothing");
}

#[test]
fn a_bad_token_is_a_usage_error_before_anything_runs() {
    let out = Command::new(exe())
        .args(["leave", "--token", "Not-A-Token"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is not a token"), "{stderr}");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_phases_refuse_to_run_off_a_mac() {
    for args in [
        &["probe"][..],
        &["leave", "--token", "t1"][..],
        &[
            "find",
            "--token",
            "t1",
            "--left-by-uid",
            "600",
            "--left-by-user",
            "u",
            "--left-by-home",
            "/h",
        ][..],
    ] {
        let out = Command::new(exe()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!(
                "js-isolation-probe: the probes run as a job on a Mac; this is {}\n",
                std::env::consts::OS
            )
        );
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn a_tool_that_ends_reports_its_exit_and_output() {
    let ran = tools(Duration::from_secs(600)).run(
        exe().to_str().unwrap(),
        &["--version"],
        b"ignored input",
    );
    assert!(ran.ok(), "{}", ran.why());
    assert!(!ran.timed_out);
    assert!(
        ran.out().starts_with("js-isolation-probe "),
        "{}",
        ran.out()
    );
    let ran = tools(Duration::from_secs(600)).run(exe().to_str().unwrap(), &["nonsense"], b"");
    assert_eq!(ran.code, Some(2));
    assert!(ran.why().starts_with("exit 2: error: "), "{}", ran.why());
}

#[test]
fn a_tool_that_never_ends_by_itself_is_killed_at_the_limit() {
    // `linger` with a day to wait does not end within this test; only the limit ends it.
    let ran = tools(Duration::from_millis(10)).run(
        exe().to_str().unwrap(),
        &["linger", "--token", "t1", "--secs", "86400"],
        b"",
    );
    assert!(ran.timed_out);
    assert_eq!(ran.code, None);
    assert_eq!(ran.why(), "timed out");
}

#[test]
fn a_tool_that_is_not_installed_cannot_start() {
    let dir = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-missing");
    let ran = tools(Duration::from_secs(1)).run(dir.join("absent").to_str().unwrap(), &[], b"");
    assert!(ran.start.is_some());
    assert!(ran.why().starts_with("cannot start: "), "{}", ran.why());
}

/// A linger starts in the background and is killed, and the thread that waits on it reaps it
/// before the test goes on, so the reaping is done (and measured) by the time the test ends.
#[test]
fn a_lingering_process_is_started_in_the_background_and_killed() {
    let dir = js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-linger");
    let tools = tools(Duration::from_secs(1));
    let pid = tools.linger("t1", 86_400).unwrap();
    assert!(pid > 0);
    tools.kill(pid).unwrap();
    assert!(
        reaped(&tools, pid),
        "the killed linger {pid} was never reaped"
    );
    assert_eq!(
        tools.kill(0).unwrap_err().raw_os_error(),
        Some(libc::ESRCH),
        "pid 0 is no process"
    );
    let missing = System {
        limit: Duration::from_secs(1),
        exe: dir.join("absent"),
    };
    assert!(missing.linger("t1", 1).is_err());
}
