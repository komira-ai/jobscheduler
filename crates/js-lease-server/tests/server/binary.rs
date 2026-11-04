//! The program itself, run as a child process: it listens, serves, and exits 0 on
//! SIGTERM after finishing; its refusals exit 2; a final save that fails exits 1.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use js_pool_fake::EXAMPLE_CONFIG;

use crate::agents_support::{JOIN, hello_body};
use crate::config_support::registering_args;
use crate::support::{Scratch, TOKEN, lease_body, request};

const PROGRAM: &str = env!("CARGO_BIN_EXE_js-lease-server");

/// How long the test waits for the program to print its address before it gives up.
/// Only a hang reaches it; a program that is working prints at once.
const START_LIMIT: Duration = Duration::from_secs(60);

/// Sends the hello of the host `gpu-01` of the pool `burst` to the program at `addr`
/// and asserts it is admitted, so a lease can be placed on it.
async fn register_host(addr: SocketAddr) {
    let hello = hello_body("gpu-01").to_string().into_bytes();
    let path = "/v1/agent/hello";
    let welcome = request(addr, "POST", path, Some(JOIN), Some(&hello)).await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
}

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
    let (mut child, addr) = launch(&registering_args(&scratch), Stdio::null());
    register_host(addr).await;

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
    let (mut child, addr) = launch(&registering_args(&scratch), Stdio::piped());
    register_host(addr).await;
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
    let mut args = registering_args(&scratch);
    let loose = scratch.write("loose", TOKEN, 0o644);
    let at = args.iter().position(|arg| arg == "--token-file").unwrap();
    args[at + 1] = loose.into_os_string();
    let out = Command::new(PROGRAM).args(&args[1..]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--token-file"), "{err}");
    assert!(out.stdout.is_empty());
}

/// The refusal of the removed hosts-file pool, as the server words it.
const MIGRATION: &str = "the agentless hosts-file pool was removed: run js-agent on each listed host and use --agent-pool-config, or a [[pool]] of kind = \"static\" (with its shape and max_machines)";

/// Runs the real program on a valid start plus `--pool-config <value>` and asserts it
/// exits 2 with nothing on standard output and exactly the migration refusal, one line,
/// on standard error.
fn assert_pool_config_refused(scratch: &Scratch, value: PathBuf) {
    let mut args = registering_args(scratch);
    args.push("--pool-config".into());
    args.push(value.into_os_string());
    let out = Command::new(PROGRAM).args(&args[1..]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        err,
        format!("js-lease-server: --pool-config: {MIGRATION}\n")
    );
    assert!(out.stdout.is_empty());
}

/// Asserts that the real program refuses the removed `--pool-config` flag even when
/// every other flag is valid: status 2, standard error exactly the one-line refusal that
/// names the flag and points to `js-agent` and `--agent-pool-config`, and nothing on
/// standard output, so neither the host list's path nor a host it names is echoed. It
/// catches a program that still reads a hosts file, that exits with another status, or
/// that prints the file's contents or any other text beside the refusal.
#[test]
fn the_program_refuses_the_removed_pool_config_flag() {
    let scratch = Scratch::new("binary_pool_config");
    let hosts = scratch.write("old-hosts.toml", EXAMPLE_CONFIG, 0o644);
    assert_pool_config_refused(&scratch, hosts);
}

/// Asserts that the real program gives the same refusal, byte for byte, when
/// `--pool-config` names a path that does not exist. It catches a program that opens the
/// path first and reports the operating system's "No such file" instead of the
/// migration.
#[test]
fn the_program_refuses_the_removed_pool_config_flag_for_a_missing_path() {
    let scratch = Scratch::new("binary_pool_config_missing");
    let missing = scratch.path("no-such-hosts.toml");
    assert_pool_config_refused(&scratch, missing);
}
