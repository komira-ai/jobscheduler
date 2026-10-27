//! The program itself, started as a process: the one place a signal is sent, and only to
//! a child process the test started.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

use rustix::process::{Pid, Signal, kill_process};

use crate::server::{POOL_OPEN_TOML, Server};

/// Asserts that the real binary, with the machine's own probes and signal handlers,
/// registers with a lease server, and on SIGTERM sends the phase `draining` and exits 0:
/// the server then lists the host as draining. It is the test of the signal handlers
/// (SIGTERM reaches the loop through the channel its sleeper waits on) and of `main`,
/// which no in-process test reaches. A GPU probe that finds no `nvidia-smi` is a warning,
/// not a failure.
#[test]
fn the_binary_registers_and_drains_on_sigterm() {
    let server = Server::start_pool("binary", &[], POOL_OPEN_TOML);
    let mut child = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args(["--server", &server.url(), "--pool", "macs"])
        .args(["--token-file", server.join_file().to_str().unwrap()])
        .args(["--host-id", "bin-1", "--label", "rack=r9"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let read = stderr.read_line(&mut line).unwrap();
        assert!(read > 0, "the agent ended before it registered: {seen}");
        seen.push_str(&line);
        if line.contains("registered, epoch 1") {
            break;
        }
    }
    assert_eq!(server.host("bin-1")["phase"], "serving");
    assert_eq!(server.host("bin-1")["labels"]["rack"], "r9");

    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let status = child.wait().unwrap();
    stderr.read_to_string(&mut seen).unwrap();
    assert!(status.success(), "{status}: {seen}");
    assert!(seen.contains("draining: the server was told"), "{seen}");
    assert_eq!(server.host("bin-1")["phase"], "draining");
}
