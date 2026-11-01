//! The program itself, run as a child process: it listens, serves, and exits 0 on
//! SIGTERM after finishing; its refusals exit 2; a final save that fails exits 1.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
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
pub(crate) struct Guard(pub(crate) Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the program on `args` (the program name first) with a timer that never fires
/// in a test, standard error going to `stderr`, and returns it with the address it
/// printed.
pub(crate) fn launch(args: &[OsString], stderr: Stdio) -> (Guard, SocketAddr) {
    let mut child = Guard(
        Command::new(PROGRAM)
            .args(&args[1..])
            .args(["--tick", "1h"])
            .stdout(Stdio::piped())
            .stderr(stderr)
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
    (child, addr)
}

/// Asserts that the real program prints the address it listens on, answers requests
/// with the system clock and a timer, and on SIGTERM stops with status 0.
#[tokio::test]
async fn the_program_serves_and_stops_on_sigterm() {
    let scratch = Scratch::new("binary");
    let (mut child, addr) = launch(&base_args(&scratch), Stdio::null());

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

/// Asserts that a program whose lease file cannot be written at the last tick exits 1
/// and says why on standard error, instead of the 0 of an orderly stop. The file is
/// made unwritable by putting a directory where its temporary file goes; a renew then
/// fails to save (a 500) and leaves the file behind the table, so the final tick has
/// to write it and cannot.
#[tokio::test]
async fn the_program_exits_1_when_the_final_save_fails() {
    let scratch = Scratch::new("binary_final_save");
    let (mut child, addr) = launch(&base_args(&scratch), Stdio::piped());
    let body = lease_body().to_string().into_bytes();
    let made = request(addr, "POST", "/v1/leases", Some(TOKEN), Some(&body)).await;
    assert_eq!(made.status, 201, "{}", made.body);
    std::fs::create_dir(scratch.path("leases.toml.tmp")).unwrap();
    let renewed = request(addr, "POST", "/v1/leases/1-1/renew", Some(TOKEN), None).await;
    assert_eq!(renewed.status, 500, "{}", renewed.body);

    let named = format!(
        "js-lease-server: the final tick could not write the lease file {}: ",
        scratch.state().display()
    );
    let pid = Pid::from_child(&child.0);
    kill_process(pid, Signal::TERM).unwrap();
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(1), "{status:?}");
    let mut message = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut message)
        .unwrap();
    assert!(message.contains(&named), "{message}");
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
