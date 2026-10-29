//! The program itself, run as a child process against the server of the test: the real
//! waiter, the real signal handlers and the process's own exit status.

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader, Read};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::process::{Command, Output};

#[cfg(target_os = "linux")]
use crate::procs::{fifo, path_str};
use crate::support::Harness;
#[cfg(target_os = "linux")]
use crate::support::run_words;

const PROGRAM: &str = env!("CARGO_BIN_EXE_js");

fn program(h: &Harness, words: &[&str]) -> Output {
    Command::new(PROGRAM)
        .args(["--server", &h.js.server])
        .arg("--token-file")
        .arg(&h.js.token_file)
        .args(words)
        .output()
        .unwrap()
}

/// Asserts that the program prints its usage on `--help` with status 0, and lists the
/// leases of the server with the status 0.
#[test]
fn the_program_prints_help_and_lists() {
    let h = Harness::new("binary_list");
    let help = Command::new(PROGRAM).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("Usage: js")
    );

    let listed = program(&h, &["--json", "lease", "list"]);
    assert_eq!(listed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap(),
        "{\"leases\":[]}\n"
    );
    let refused = program(&h, &["lease", "renew", "9-9"]);
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(refused.stderr).unwrap(),
        "js: refused: unknown-lease\n"
    );
}

/// Asserts that a SIGTERM sent to the program while it runs a command (the command sends
/// it, to its parent) is forwarded to the command, the program exits 143 and the lease is
/// released, and that the program renews through its own waiter without waiting for it.
#[test]
fn the_program_forwards_sigterm_and_releases() {
    let h = Harness::new("binary_run");
    at_wall_time(&h);
    let script = "kill -TERM $PPID; exec sleep 600";
    let ran = program(
        &h,
        &[
            "lease",
            "run",
            "--class",
            "gpu-host",
            "--gpus",
            "1",
            "--gpu-class",
            "nvidia-a100",
            "--max-wall",
            "2h",
            "--ttl",
            "60s",
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    assert_eq!(
        ran.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        String::from_utf8(ran.stderr)
            .unwrap()
            .starts_with("lease 1-1 host gpu-01 ")
    );
    assert!(h.listed().is_empty());
}

/// Asserts that a `js` that is killed (SIGKILL, which no handler can see) takes its
/// command with it: the command, a shell that would leave a mark after twenty seconds,
/// is killed with `js`, and the pipe it held open reaches its end with no mark left.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_js_takes_its_command_with_it() {
    let h = Harness::new("binary_killed");
    at_wall_time(&h);
    let ready = fifo(&h.scratch, "ready");
    let mark = h.scratch.path("survived");
    let script = r#"exec 3> "$0"; echo up >&3; sleep 20 3>&-; echo survived > "$1""#;
    let words = run_words(
        &["--ttl", "60s"],
        &["sh", "-c", script, path_str(&ready), path_str(&mark)],
    );
    let mut js = Command::new(PROGRAM)
        .args(["--server", &h.js.server])
        .arg("--token-file")
        .arg(&h.js.token_file)
        .args(words)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pipe = BufReader::new(File::open(&ready).unwrap());
    let mut up = String::new();
    pipe.read_line(&mut up).unwrap();
    js.kill().unwrap();
    js.wait().unwrap();
    let mut rest = String::new();
    pipe.read_to_string(&mut rest).unwrap();
    assert_eq!((up.as_str(), rest.as_str()), ("up\n", ""));
    assert!(!mark.exists());
}

/// Sets the clock of the test server to the system's, which the program reads: the
/// expiry times the server answers with are read against the program's own clock.
fn at_wall_time(h: &Harness) {
    h.clock.set(js_lease::Clock::now(&js_lease::SystemClock));
}
