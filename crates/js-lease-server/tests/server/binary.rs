//! The program itself, run as a child process: it listens, serves, and exits 0 on
//! SIGTERM after finishing; and its refusals exit 2.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use crate::support::{Scratch, TOKEN, base_args, lease_body, request};

const PROGRAM: &str = env!("CARGO_BIN_EXE_js-lease-server");

/// How long the test waits for the program to print its address before it gives up.
/// Only a hang reaches it; a program that is working prints at once.
const START_LIMIT: Duration = Duration::from_secs(60);

/// A child process that is killed and reaped when the guard is dropped, so a failed
/// assertion leaves no server behind holding its files.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Asserts that the real program prints the address it listens on, answers requests
/// with the system clock and a timer, and on SIGTERM stops with status 0.
#[tokio::test]
async fn the_program_serves_and_stops_on_sigterm() {
    let scratch = Scratch::new("binary");
    let args = base_args(&scratch);
    let mut child = Guard(
        Command::new(PROGRAM)
            .args(&args[1..])
            .args(["--tick", "1h"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (first, line) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stdout).read_line(&mut text);
        let _ = first.send(text);
    });
    let line = line.recv_timeout(START_LIMIT).unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap()
        .parse()
        .unwrap();

    let health = request(addr, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    let body = lease_body().to_string().into_bytes();
    let made = request(addr, "POST", "/v1/leases", Some(TOKEN), Some(&body)).await;
    assert_eq!(made.status, 201, "{}", made.body);

    let pid = Pid::from_child(&child.0);
    kill_process(pid, Signal::TERM).unwrap();
    let status = child.0.wait().unwrap();
    assert!(status.success(), "{status:?}");
    let saved = std::fs::read_to_string(scratch.state()).unwrap();
    assert!(saved.contains("lease_count = 1"));
}

/// Asserts that the real program exits 2 and names the flag when a start is refused.
#[test]
fn the_program_refuses_a_loose_token_file() {
    let scratch = Scratch::new("binary_refusal");
    let mut args = base_args(&scratch);
    let loose = scratch.write("loose", TOKEN, 0o644);
    let at = args.iter().position(|arg| arg == "--token-file").unwrap();
    args[at + 1] = loose.into_os_string();
    let out = Command::new(PROGRAM).args(&args[1..]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--token-file"), "{err}");
    assert!(out.stdout.is_empty());
}
