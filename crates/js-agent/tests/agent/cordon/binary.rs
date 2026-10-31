//! The program itself, started as a process with the verb: the one place the verb's own
//! signal handling and system clock run, and the only signal sent is to a child the test
//! started.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

use super::http::enrol;

/// How long the test waits for the next line of the verb before it fails. It is a guard
/// against a hang, never an expectation: a healthy verb prints within milliseconds, and the
/// test asserts on what was printed, not on how soon.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The verb's process. Dropping it ends the process and collects it, so a test that fails
/// before it signals the child does not leave a cordon being renewed.
struct Verb {
    child: Child,
}

impl Drop for Verb {
    fn drop(&mut self) {
        // Both fail only when the child is already gone and collected.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The next line of standard output, or `None` once the verb has closed it.
fn next_line(lines: &Receiver<String>) -> Option<String> {
    match lines.recv_timeout(HANG_GUARD) {
        Ok(line) => Some(line),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => panic!("the verb said nothing for a long time"),
    }
}

/// Asserts the program end to end: `js-agent cordon` finds the credential in the state root,
/// places a cordon on the real server (which then shows it), prints `cordoned` and `idle`,
/// and on SIGTERM lifts it, prints `lifted`, exits 0 with nothing on standard error and
/// leaves the server with no cordon. It is the test of the verb's dispatch before the
/// daemon's signal handling, of its own handling of SIGTERM, of the system clock and of the
/// real connector. It catches a verb that dies on the signal with the cordon left in force.
#[test]
fn the_binary_cordons_a_host_and_lifts_the_cordon_on_sigterm() {
    let enrolled = enrol("cordon_binary");
    let (url, state) = (enrolled.server.url(), enrolled.state_flag());
    let child = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args(["cordon", "--server", &url, "--state-root", &state])
        .args(["--host-id", "mac-1", "--for", "1h", "--lease-secs", "30"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut verb = Verb { child };
    let pid = Pid::from_raw(i32::try_from(verb.child.id()).unwrap()).unwrap();
    let stdout = BufReader::new(verb.child.stdout.take().unwrap());
    let (send, lines) = channel();
    thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });

    let cordoned = next_line(&lines).expect("the verb ended before it cordoned");
    assert_eq!(cordoned, "cordoned host=mac-1 expires_in_ms=30000 leases=0");
    assert_eq!(next_line(&lines).as_deref(), Some("idle host=mac-1"));
    let listed = enrolled.server.host("mac-1");
    assert!(listed["cordon"]["expires_at_ms"].is_u64(), "{listed}");

    kill_process(pid, Signal::TERM).unwrap();
    assert_eq!(next_line(&lines).as_deref(), Some("lifted host=mac-1"));
    assert_eq!(
        next_line(&lines),
        None,
        "the verb closes its output at exit"
    );
    let status = verb.child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut err = String::new();
    verb.child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(err, "");
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
}
