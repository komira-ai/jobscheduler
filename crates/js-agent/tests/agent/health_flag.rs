//! `--health-listen`: the address it refuses, and the route the running agent answers on.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use js_agent::{Dialer, Services, Sleeper, Wake, run_with};
use serde_json::json;

use crate::bounds::WakeBudget;
use crate::bundle::fake::FakeHost;
use crate::jobs::fake::FakeMachine;
use crate::support::{
    Lowest, Script, ScriptConnector, beat_ok, code, flags, linux_host, no_job_runtime, reply,
    run_scripted, welcome,
};

/// A log that the sleeper of the test can read while the agent writes it.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).unwrap()
    }
}

/// The address the agent printed for its route: the text after `health route: http://` up to
/// the path.
fn route_address(log: &str) -> String {
    let (_, rest) = log.split_once("health route: http://").unwrap();
    rest.split_once('/').unwrap().0.to_owned()
}

/// What the route at `address` answers to a `GET` of `path`.
fn get(address: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    text
}

/// A sleeper that asks the route at every wait, once the agent has said where it is, and
/// stops the agent at its `stop_at`th wait.
struct Probe {
    log: Shared,
    answers: Rc<RefCell<Vec<String>>>,
    stop_at: usize,
    budget: WakeBudget,
}

impl Sleeper for Probe {
    fn sleep(&mut self, _: Duration) -> Wake {
        self.budget.spend();
        let address = route_address(&self.log.text());
        let mut answers = self.answers.borrow_mut();
        answers.push(get(&address, "/healthz"));
        if answers.len() == 1 {
            answers.push(get(&address, "/metrics"));
        }
        match answers.len() > self.stop_at {
            true => Wake::Stop,
            false => Wake::Elapsed,
        }
    }
}

/// Asserts that an address that is not a loopback one, one that is taken and one that is not
/// an address end the agent with status 2 before anything is sent, each naming the flag. It
/// catches a route open to the network, a bind that fails without a word, and an agent that
/// registers and then finds it cannot say how healthy it is.
#[test]
fn an_address_that_is_not_loopback_or_is_taken_or_is_no_address_ends_the_agent() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap().to_string();
    let cases = [
        (
            "192.0.2.1:7311",
            "--health-listen: 192.0.2.1:7311 is not a loopback address",
        ),
        (
            "0.0.0.0:0",
            "--health-listen: 0.0.0.0:0 is not a loopback address",
        ),
        (busy.as_str(), "--health-listen: cannot listen on"),
        ("not-an-address", "--health-listen"),
    ];
    for (address, wanted) in cases {
        let (ran, sent, waits) = run_scripted(
            &["--health-listen", address],
            &linux_host(),
            vec![welcome(1)],
            1,
        );
        assert_eq!(ran.status(), code(2), "{address}: {}", ran.err);
        assert!(ran.err.contains(wanted), "{address}: {}", ran.err);
        assert!(sent.borrow().is_empty(), "{address}: something was sent");
        assert!(waits.is_empty());
    }
}

/// Asserts the route of a running agent over a loopback socket bound to port 0: the agent
/// says where it listens, the route answers `200` and `placeable` once the session is admitted,
/// `503` with the reason, check and run of the taint the server's reply names, and `200` again
/// when a later reply names none, `404` for another path, and the log holds one line for each
/// change of the verdict. It catches a flag that binds nothing, a route that is not fed by the
/// session, and a verdict that is not in the journal.
#[test]
fn the_agent_serves_its_verdict_on_the_loopback_address_it_was_given() {
    let held = reply(
        200,
        json!({ "host_taint": { "reason": "residue", "run_id": "r1", "check": "daemonised-child" } }),
    );
    let (script, _) = Script::new(vec![welcome(1), held, beat_ok()]);
    let log = Shared::default();
    let answers = Rc::default();
    let mut probe = Probe {
        log: log.clone(),
        answers: Rc::clone(&answers),
        stop_at: 3,
        budget: WakeBudget::new(),
    };
    let host = linux_host();
    let connector = ScriptConnector::of(script);
    let mut services = Services {
        platform: host.platform.clone(),
        commands: &host.commands,
        files: &host.files,
        connector: &connector,
        dispatch: &Dialer,
        jitter: &mut Lowest,
        sleeper: &mut probe,
        machine: Arc::new(FakeMachine::new()),
        bundle: FakeHost::new(),
        long_bundle_values: false,
        worker_hook: None,
        rescue_bound: None,
        euid: host.euid,
        ancestors_floor: &host.ancestors_floor,
        secret_owner: host.secret_owner,
        xcode_site: host.site(),
    };
    let args = flags(&["--health-listen", "127.0.0.1:0"]);
    let named = no_job_runtime(&args).iter().chain(&args).copied();
    let argv = std::iter::once("js-agent").chain(named);
    let mut out = Shared::default();
    let mut err = log.clone();
    let status = run_with(
        argv.map(Into::into).collect(),
        &mut out,
        &mut err,
        &mut services,
    );
    assert_eq!(format!("{status:?}"), code(0), "{}", log.text());

    let answers = answers.borrow();
    let placeable = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\n\
                     Connection: close\r\n\r\nplaceable\n";
    assert_eq!(answers[0], placeable);
    assert!(
        answers[1].starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{}",
        answers[1]
    );
    assert!(
        answers[2].starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{}",
        answers[2]
    );
    assert!(
        answers[2].ends_with(
            "\r\n\r\nnot placeable: the lease server holds a taint on this host \
             (reason residue, check daemonised-child, run r1)\n"
        ),
        "{}",
        answers[2]
    );
    assert_eq!(answers[3], placeable);
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("js-agent: health: "))
        .collect();
    assert_eq!(
        lines,
        [
            "js-agent: health: not placeable: no session with the lease server",
            "js-agent: health: placeable",
            "js-agent: health: not placeable: the lease server holds a taint on this host \
             (reason residue, check daemonised-child, run r1)",
            "js-agent: health: placeable",
        ]
    );
}

/// Asserts that an agent started without `--health-listen` prints no health line and binds
/// nothing. It catches a route or a log line that appears for every agent.
#[test]
fn without_the_flag_the_agent_has_no_route_and_logs_no_verdict() {
    let (ran, _, _) = run_scripted(&[], &linux_host(), vec![welcome(1), beat_ok()], 2);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran.err.contains("health"), "{}", ran.err);
}
