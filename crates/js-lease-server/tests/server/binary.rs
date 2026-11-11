//! The program itself, run as a child process: it listens, serves, and exits 0 on
//! SIGTERM after finishing; its refusals exit 2.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use crate::support::{Scratch, TOKEN, base_args, request};

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

/// The lines a child process printed on one of its streams, read as they come.
pub(crate) struct Lines(std::sync::mpsc::Receiver<String>);

impl Lines {
    /// The lines `stream` yields, from a thread that reads it to its end.
    pub(crate) fn of(stream: impl std::io::Read + Send + 'static) -> Self {
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self(lines)
    }

    /// The next line. Only a hang reaches the limit: a program that is working prints at once.
    pub(crate) fn next(&self) -> String {
        self.0
            .recv_timeout(START_LIMIT)
            .expect("the program printed a line")
    }

    /// The first line from here on that contains `needle`.
    pub(crate) fn until(&self, needle: &str) -> String {
        loop {
            let line = self.next();
            if line.contains(needle) {
                return line;
            }
        }
    }

    /// Every line left, read to the end of the stream: call it once the program has ended.
    pub(crate) fn rest(&self) -> Vec<String> {
        self.0.iter().collect()
    }
}

/// Starts the program on `args` (the program name first) with a timer that never fires
/// in a test, standard error going to `stderr`, and returns it with what it prints on
/// standard output.
pub(crate) fn spawn(args: &[OsString], stderr: Stdio) -> (Guard, Lines) {
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
    (child, Lines::of(stdout))
}

/// The address a line of the form `<prefix><address>` names.
pub(crate) fn address_in(line: &str, prefix: &str) -> SocketAddr {
    line.trim().strip_prefix(prefix).unwrap().parse().unwrap()
}

/// [`launch`], and the address of the readiness route as well when the program has one
/// (`--ready-listen`), which it prints before the address it listens on.
pub(crate) fn launch_with_ready(
    args: &[OsString],
    stderr: Stdio,
) -> (Guard, SocketAddr, Option<SocketAddr>) {
    let (child, lines) = spawn(args, stderr);
    let mut line = lines.next();
    let ready = line
        .starts_with("ready on ")
        .then(|| address_in(&line, "ready on "));
    if ready.is_some() {
        line = lines.next();
    }
    (child, address_in(&line, "listening on "), ready)
}

/// Starts the program on `args` (the program name first) with a timer that never fires
/// in a test, standard error going to `stderr`, and returns it with the address it
/// printed. A line that names the readiness address before it is skipped.
pub(crate) fn launch(args: &[OsString], stderr: Stdio) -> (Guard, SocketAddr) {
    let (child, listen, _) = launch_with_ready(args, stderr);
    (child, listen)
}

/// Asserts that the real program prints the address it listens on, answers requests
/// with the system clock and a timer, and on SIGTERM stops with status 0.
#[tokio::test]
async fn the_program_serves_and_stops_on_sigterm() {
    let scratch = Scratch::new("binary");
    let (mut child, addr) = launch(&base_args(&scratch), Stdio::null());

    let health = request(addr, "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    let hosts = request(addr, "GET", "/v1/hosts", Some(TOKEN), None).await;
    assert_eq!(hosts.status, 200, "{}", hosts.body);

    let pid = Pid::from_child(&child.0);
    kill_process(pid, Signal::TERM).unwrap();
    let status = child.0.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(!scratch.state().exists(), "the program made no lease file");
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
