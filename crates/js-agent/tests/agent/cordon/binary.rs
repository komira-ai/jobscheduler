//! The program itself, started as a process with the verb: the one place the verb's own
//! signal handling and system clock run, and the only signal sent is to a child the test
//! started.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use js_types::CordonHolder;
use rustix::process::{Pid, Signal, kill_process};

use super::http::{as_host, enrol};
use crate::server::Server;

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

/// A running verb, the lines it prints and the server it cordons on.
struct Running {
    verb: Verb,
    pid: Pid,
    lines: Receiver<String>,
}

/// Starts `js-agent cordon` on `server` for the host `mac-1`, with its certificate, and `more`
/// flags beside the ones every run needs.
fn start(server: &Server, more: &[&str]) -> Running {
    let child = Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .arg("cordon")
        .args(as_host(server))
        .args(["--for", "1h", "--lease-secs", "30"])
        .args(more)
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
    Running { verb, pid, lines }
}

/// Reads what is left of the run once it has been signalled: the `lifted` line, the end of
/// standard output, a status of 0 and nothing on standard error.
fn finish_lifted(mut run: Running) {
    assert_eq!(next_line(&run.lines).as_deref(), Some("lifted host=mac-1"));
    assert_eq!(
        next_line(&run.lines),
        None,
        "the verb closes its output at exit"
    );
    let status = run.verb.child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut err = String::new();
    run.verb
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(err, "");
}

/// Asserts the program end to end: `js-agent cordon` presents the host's certificate and key,
/// places a cordon on the real server (which then shows it), prints `cordoned` with the holder
/// it made for the run (`cordon-<its pid>-<unix millis>`, a valid holder id) and `idle`, and
/// on SIGTERM lifts it, prints `lifted`, exits 0 with nothing on standard error and leaves the
/// server with no cordon. It is the test of the verb's dispatch before the daemon's signal
/// handling, of its own handling of SIGTERM, of the system clock, of the default holder and of
/// the real connector. It catches a verb that dies on the signal with the cordon left in
/// force, and a default holder that is not a valid id or is not made per run.
#[test]
fn the_binary_cordons_a_host_and_lifts_the_cordon_on_sigterm() {
    let enrolled = enrol("cordon_binary");
    let run = start(&enrolled.server, &[]);

    let cordoned = next_line(&run.lines).expect("the verb ended before it cordoned");
    let prefix = format!("cordoned host=mac-1 holder=cordon-{}-", run.verb.child.id());
    let rest = cordoned.strip_prefix(&prefix).expect(&cordoned);
    let (millis, tail) = rest.split_once(' ').expect(&cordoned);
    assert!(millis.bytes().all(|b| b.is_ascii_digit()), "{cordoned}");
    assert_eq!(tail, "expires_in_ms=30000 leases=0");
    let holder = format!("cordon-{}-{millis}", run.verb.child.id());
    assert!(CordonHolder::new(&holder).is_ok(), "{holder}");
    assert_eq!(next_line(&run.lines).as_deref(), Some("idle host=mac-1"));
    let listed = enrolled.server.host("mac-1");
    assert!(listed["cordon"]["expires_at_ms"].is_u64(), "{listed}");

    kill_process(run.pid, Signal::TERM).unwrap();
    finish_lifted(run);
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
}

/// Asserts that SIGHUP, which ends an ssh session, stops the verb as SIGTERM does: it lifts its
/// cordon, prints `lifted`, exits 0 and leaves the server with no cordon, with the holder it
/// was given printed on the `cordoned` line. It catches a verb that the hangup kills (or
/// leaves running) with the cordon in force.
#[test]
fn the_binary_lifts_the_cordon_on_sighup() {
    let enrolled = enrol("cordon_binary_hup");
    let run = start(&enrolled.server, &["--holder", "ops-session"]);

    let cordoned = next_line(&run.lines).expect("the verb ended before it cordoned");
    assert_eq!(
        cordoned,
        "cordoned host=mac-1 holder=ops-session expires_in_ms=30000 leases=0"
    );
    assert_eq!(next_line(&run.lines).as_deref(), Some("idle host=mac-1"));

    kill_process(run.pid, Signal::HUP).unwrap();
    finish_lifted(run);
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
}
