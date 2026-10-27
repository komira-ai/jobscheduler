//! The program itself, started as a process: the one place a signal is sent, and only to
//! a child process the test started.

use std::io::{BufRead, BufReader, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Child, ChildStderr, Command, Stdio};

use rustix::process::{Pid, Signal, kill_process, test_kill_process};

use crate::server::{POOL_OPEN_TOML, Server};

/// The most stderr lines the test reads while it waits for the agent to register: the
/// agent says one line per attempt, so a hello that is refused for good ends the wait
/// with a failure instead of blocking the test forever.
const REGISTER_LINES: usize = 200;

/// The agent process the test started. Dropping it ends the process and collects it, so
/// a test that fails before it signals the child does not leave an agent running.
struct Agent {
    child: Child,
}

impl Agent {
    fn start(server: &Server, host: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_js-agent"))
            .args(["--server", &server.url(), "--pool", "macs"])
            .args(["--token-file", server.join_file().to_str().unwrap()])
            .args(["--host-id", host, "--label", "rack=r9"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child }
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.child.id()).unwrap()).unwrap()
    }

    fn stderr(&mut self) -> BufReader<ChildStderr> {
        BufReader::new(self.child.stderr.take().unwrap())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Both fail only when the child is already gone and collected.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Asserts that the real binary, with the machine's own probes and signal handlers,
/// registers with a lease server, and on SIGTERM sends the phase `draining` and exits 0:
/// the server then lists the host as draining. It is the test of the signal handlers
/// (SIGTERM reaches the loop through the channel its sleeper waits on) and of `main`,
/// which no in-process test reaches. A GPU probe that finds no `nvidia-smi` is a warning,
/// not a failure.
#[test]
fn the_binary_registers_and_drains_on_sigterm() {
    let server = Server::start_pool("binary", &[], POOL_OPEN_TOML);
    let mut agent = Agent::start(&server, "bin-1");
    let mut stderr = agent.stderr();
    let mut seen = String::new();
    let mut registered = false;
    for _ in 0..REGISTER_LINES {
        let mut line = String::new();
        let read = stderr.read_line(&mut line).unwrap();
        assert!(read > 0, "the agent ended before it registered: {seen}");
        seen.push_str(&line);
        if line.contains("registered, epoch 1") {
            registered = true;
            break;
        }
    }
    assert!(
        registered,
        "no registration in {REGISTER_LINES} lines: {seen}"
    );
    assert_eq!(server.host("bin-1")["phase"], "serving");
    assert_eq!(server.host("bin-1")["labels"]["rack"], "r9");

    kill_process(agent.pid(), Signal::TERM).unwrap();
    let status = agent.child.wait().unwrap();
    stderr.read_to_string(&mut seen).unwrap();
    assert!(status.success(), "{status}: {seen}");
    assert!(seen.contains("draining: the server was told"), "{seen}");
    assert_eq!(server.host("bin-1")["phase"], "draining");
}

/// Asserts that a test which fails while the agent runs leaves no process behind: the
/// guard's drop, run by the unwinding of a failed assertion, ends the child and collects
/// it, so signal 0 to its pid finds no process. It catches a guard that only drops the
/// handle, which leaves an agent heartbeating to a server that is gone.
#[test]
fn a_failing_test_does_not_leave_the_agent_running() {
    let server = Server::start_pool("binary_guard", &[], POOL_OPEN_TOML);
    let mut pid = None;
    let failed = catch_unwind(AssertUnwindSafe(|| {
        let agent = Agent::start(&server, "bin-2");
        pid = Some(agent.pid());
        assert_eq!(
            server.host("bin-2")["phase"],
            "never serving: the test fails"
        );
    }));
    assert!(failed.is_err(), "the assertion inside must fail");
    assert!(
        test_kill_process(pid.unwrap()).is_err(),
        "the agent is still there"
    );
}
