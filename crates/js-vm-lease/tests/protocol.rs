//! The line protocol between the agent and `js-vmm lease`: [`serve`] over the recording
//! driver, reached through a [`Session`] on a pair of Unix sockets (anonymous, no path).

use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;

use js_vm_lease::fake::{Call, FakeDriver};
use js_vm_lease::protocol::{Reply, Request, Session, serve};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmError, VmName, VmSize};
use serde_json::json;

fn vm(name: &str) -> VmName {
    VmName::new(name).unwrap()
}

/// `driver` served on a thread, and a session to it.
fn served(driver: &Arc<FakeDriver>) -> (Arc<Session>, JoinHandle<io::Result<()>>) {
    let (agent, helper) = UnixStream::pair().unwrap();
    let driver = Arc::clone(driver);
    let service = std::thread::spawn(move || {
        let input = BufReader::new(helper.try_clone()?);
        let mut output = helper;
        serve(&*driver, input, &mut output)
    });
    let requests = Box::new(agent.try_clone().unwrap());
    let session = Session::new(requests, Box::new(BufReader::new(agent)));
    (Arc::new(session), service)
}

/// Asserts that every request is answered in order with what the driver said (done, the
/// command's end, the output, the names, the driver's error), that a kill sent while a
/// command waits ends it at once, and that the service ends when the session does. Catches
/// a kill queued behind the command it is meant to end (the stop would never arrive), and a
/// reply that loses the driver's error.
#[test]
fn every_request_is_answered_in_order_and_a_kill_acts_at_once() {
    let driver = Arc::new(
        FakeDriver::new()
            .holding()
            .answering(ExecEnd::Exited(0), "out", "err")
            .leaving(&[vm("old")])
            .failing(Step::Boot),
    );
    let (session, service) = served(&driver);
    let one = vm("run-1");
    let size = VmSize {
        cpus: 2,
        memory_mib: 4096,
    };
    let call = |request: Request| session.call(&request).unwrap();
    assert_eq!(call(Request::Clone { vm: one.clone() }), Reply::Done);
    assert_eq!(
        call(Request::Boot {
            vm: one.clone(),
            size
        }),
        Reply::Failed {
            error: VmError::step(&one, Step::Boot, "the fake was told to fail")
        }
    );
    assert_eq!(
        call(Request::Boot {
            vm: one.clone(),
            size
        }),
        Reply::Done
    );
    assert_eq!(call(Request::WaitReady { vm: one.clone() }), Reply::Done);
    let request = ExecRequest {
        argv: vec!["make".to_owned()],
        env: Vec::new(),
        timeout_ms: 1000,
    };
    let running = {
        let (session, run) = (
            Arc::clone(&session),
            Request::RunCommand {
                vm: one.clone(),
                request: request.clone(),
            },
        );
        std::thread::spawn(move || session.call(&run).unwrap())
    };
    driver.wait_for(&one, Step::Exec);
    session.send(&Request::Kill { vm: one.clone() }).unwrap();
    assert_eq!(
        running.join().unwrap(),
        Reply::Ended {
            end: ExecEnd::Killed
        }
    );
    assert_eq!(
        call(Request::Collect {
            vm: one.clone(),
            tail_bytes: 2
        }),
        Reply::Collected {
            output: Collected {
                stdout: b"ut".to_vec(),
                stderr: b"rr".to_vec(),
                truncated: true,
            }
        }
    );
    assert_eq!(call(Request::Stop { vm: one.clone() }), Reply::Done);
    assert_eq!(call(Request::Destroy { vm: one.clone() }), Reply::Done);
    assert_eq!(
        call(Request::Leftovers),
        Reply::Names {
            names: vec![vm("old")]
        }
    );
    drop(session);
    service.join().unwrap().unwrap();
    let calls = driver.calls();
    let kill = calls.iter().position(|c| *c == Call::Kill(one.clone()));
    let exec = calls.iter().position(|c| matches!(c, Call::Exec(..)));
    assert!(kill > exec, "{calls:?}");
}

/// Asserts that a line that is no request is answered `refused` with the decoder's reason
/// and that the service goes on, and that a request naming no valid VM is refused too.
/// Catches one bad line that ends the service (every VM of the helper would be lost) and a
/// path smuggled in as a VM name.
#[test]
fn a_line_that_is_no_request_is_refused_and_the_service_goes_on() {
    let driver = Arc::new(FakeDriver::new());
    let (mut agent, helper) = UnixStream::pair().unwrap();
    let service = {
        let driver = Arc::clone(&driver);
        std::thread::spawn(move || {
            let input = BufReader::new(helper.try_clone()?);
            let mut output = helper;
            serve(&*driver, input, &mut output)
        })
    };
    agent
        .write_all(b"nonsense\n{\"op\":\"stop\",\"vm\":\"../etc\"}\n{\"op\":\"leftovers\"}\n")
        .unwrap();
    agent.shutdown(std::net::Shutdown::Write).unwrap();
    let replies: Vec<Reply> = BufReader::new(agent)
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    assert!(
        matches!(&replies[0], Reply::Refused { why } if why == "expected ident at line 1 column 2"),
        "{replies:?}"
    );
    assert!(
        matches!(&replies[1], Reply::Refused { why } if why.contains("is not a VM name")),
        "{replies:?}"
    );
    assert_eq!(replies[2], Reply::Names { names: Vec::new() });
    service.join().unwrap().unwrap();
    assert_eq!(driver.calls(), [Call::Leftovers]);
}

/// A writer that cannot be written.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the agent is gone"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that fails.
struct Unreadable;

impl Read for Unreadable {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("the pipe broke"))
    }
}

/// Asserts that the service ends with an error when a reply cannot be written or the input
/// cannot be read. Catches a helper that spins answering into nothing.
#[test]
fn the_service_ends_when_it_cannot_write_or_read() {
    let driver = FakeDriver::new();
    let input = Cursor::new(b"{\"op\":\"leftovers\"}\n".to_vec());
    let wrote = serve(&driver, input, &mut Broken).unwrap_err();
    assert_eq!(wrote.to_string(), "the agent is gone");
    let read = serve(&driver, BufReader::new(Unreadable), &mut Vec::new()).unwrap_err();
    assert_eq!(read.to_string(), "the pipe broke");
}

/// Asserts that a session reports a helper that ended before it answered, and a reply that
/// does not decode. Catches a dead helper read as a VM that answered.
#[test]
fn a_session_reports_an_ended_helper_and_a_bad_reply() {
    let ended = Session::new(Box::new(Vec::new()), Box::new(Cursor::new(Vec::new())));
    let error = ended.call(&Request::Leftovers).unwrap_err();
    assert_eq!(
        (error.kind(), error.to_string()),
        (
            io::ErrorKind::UnexpectedEof,
            "the VM helper ended".to_owned()
        )
    );
    let garbled = Session::new(
        Box::new(Vec::new()),
        Box::new(Cursor::new(b"garbage\n".to_vec())),
    );
    assert!(garbled.call(&Request::Leftovers).is_err());
    let unsendable = Session::new(Box::new(Broken), Box::new(Cursor::new(Vec::new())));
    assert_eq!(
        unsendable
            .send(&Request::Leftovers)
            .unwrap_err()
            .to_string(),
        "the agent is gone"
    );
}

/// Asserts the spelling of the lines on the wire between two binaries of one release (the
/// agent and `js-vmm`): each request is an object tagged `op`, each reply one tagged
/// `reply`. Catches a rename that would leave an agent and its `js-vmm` speaking past each
/// other.
#[test]
fn the_lines_are_spelled_as_documented() {
    let boot = Request::Boot {
        vm: vm("run-1"),
        size: VmSize {
            cpus: 2,
            memory_mib: 4096,
        },
    };
    assert_eq!(
        serde_json::to_value(&boot).unwrap(),
        json!({ "op": "boot", "vm": "run-1", "size": { "cpus": 2, "memory_mib": 4096 } })
    );
    assert_eq!(
        serde_json::to_value(Request::Leftovers).unwrap(),
        json!({ "op": "leftovers" })
    );
    assert_eq!(
        serde_json::to_value(Reply::Ended {
            end: ExecEnd::Exited(3)
        })
        .unwrap(),
        json!({ "reply": "ended", "end": { "Exited": 3 } })
    );
    let failed = Reply::Failed {
        error: VmError::step(&vm("run-1"), Step::Ready, "late"),
    };
    let text = serde_json::to_string(&failed).unwrap();
    assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), failed);
}
