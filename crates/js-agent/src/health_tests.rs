use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use js_agent_wire::Taint;

use super::{Health, NO_SESSION, OWN_TAINT, REQUEST_CAP, Route, Verdict, request_line, respond};

fn taint(reason: &str, check: Option<&str>, run: Option<&str>) -> Taint {
    Taint {
        reason: reason.to_owned(),
        run_id: run.map(str::to_owned),
        check: check.map(str::to_owned),
    }
}

fn residue() -> Taint {
    taint("residue", Some("daemonised-child"), Some("run-1"))
}

const SERVER_RESIDUE: &str = "the lease server holds a taint on this host (reason residue, check daemonised-child, run run-1)";

fn not_placeable(why: &str) -> Verdict {
    Verdict::NotPlaceable(why.to_owned())
}

/// A health state that was told of a welcome, with the lines up to then taken.
fn admitted() -> Health {
    let health = Health::default();
    health.admitted(true);
    health.take_lines();
    health
}

/// Asserts that a health state nobody has told anything is not placeable for want of a session,
/// and that the first line says so and is printed once. It catches a host that looks well
/// before the session has been admitted, and a first verdict that never reaches the log.
#[test]
fn a_new_health_is_not_placeable_for_want_of_a_session_and_says_so_once() {
    let health = Health::default();
    assert_eq!(health.verdict(), not_placeable(NO_SESSION));
    assert_eq!(
        health.take_lines(),
        ["health: not placeable: no session with the lease server"]
    );
    assert!(health.take_lines().is_empty());
}

/// Asserts that an admitted host with no taint is placeable, and that the welcome is one line.
/// It catches a verdict that stays unhealthy once the session is admitted.
#[test]
fn an_admitted_host_with_no_taint_is_placeable() {
    let health = Health::default();
    health.take_lines();
    health.admitted(true);
    assert_eq!(health.verdict(), Verdict::Placeable);
    assert_eq!(health.take_lines(), ["health: placeable"]);
}

/// Asserts that the lease server's taint makes the host not placeable and that the reason
/// names the taint's reason, check and run id, and only those it has. It catches a taint shown
/// without its cause, which leaves the operator to look the host up on the server.
#[test]
fn the_lease_servers_taint_names_its_reason_check_and_run() {
    let health = admitted();
    health.server_taint(Some(residue()));
    assert_eq!(health.verdict(), not_placeable(SERVER_RESIDUE));
    assert_eq!(
        health.take_lines(),
        [format!("health: not placeable: {SERVER_RESIDUE}")]
    );
    health.server_taint(Some(taint("host-job", None, None)));
    assert_eq!(
        health.verdict(),
        not_placeable("the lease server holds a taint on this host (reason host-job)")
    );
}

/// Asserts that the agent's own taint makes the host not placeable, with its own reason. It
/// catches a tainted agent that reports itself well.
#[test]
fn the_agents_own_taint_makes_the_host_not_placeable() {
    let health = admitted();
    health.own_taint(true);
    assert_eq!(health.verdict(), not_placeable(OWN_TAINT));
    assert_eq!(
        health.take_lines(),
        [format!("health: not placeable: {OWN_TAINT}")]
    );
}

/// Asserts the order of the reasons: no session before the server's taint before the agent's
/// own, and that the host is placeable again once every one of them is gone. It catches a
/// reason that hides a more basic one, and a taint that is never forgotten.
#[test]
fn the_reasons_come_in_order_and_the_host_is_placeable_when_all_are_gone() {
    let health = Health::default();
    health.server_taint(Some(residue()));
    health.own_taint(true);
    assert_eq!(health.verdict(), not_placeable(NO_SESSION));
    health.admitted(true);
    assert_eq!(health.verdict(), not_placeable(SERVER_RESIDUE));
    health.server_taint(None);
    assert_eq!(health.verdict(), not_placeable(OWN_TAINT));
    health.own_taint(false);
    assert_eq!(health.verdict(), Verdict::Placeable);
    health.admitted(false);
    assert_eq!(health.verdict(), not_placeable(NO_SESSION));
}

/// Asserts that a line is kept for each change of the verdict and for nothing else: the same
/// verdict told again, and a taint that is hidden behind a reason that already holds, add no
/// line, and a change of the reason's text adds one. It catches a log that repeats itself on
/// every heartbeat.
#[test]
fn one_line_is_kept_for_each_change_of_the_verdict() {
    let health = Health::default();
    health.admitted(true);
    health.admitted(true);
    health.own_taint(false);
    health.server_taint(Some(residue()));
    health.server_taint(Some(residue()));
    health.own_taint(true);
    health.server_taint(Some(taint("host-job", None, None)));
    health.server_taint(None);
    health.own_taint(false);
    let lines = health.take_lines();
    let expected = [
        "health: not placeable: no session with the lease server".to_owned(),
        "health: placeable".to_owned(),
        format!("health: not placeable: {SERVER_RESIDUE}"),
        "health: not placeable: the lease server holds a taint on this host (reason host-job)"
            .to_owned(),
        format!("health: not placeable: {OWN_TAINT}"),
        "health: placeable".to_owned(),
    ];
    assert_eq!(lines, expected);
}

/// A reader that gives its chunks in turn, an error where a chunk is an error, and end of
/// input after them.
struct Chunks(VecDeque<io::Result<Vec<u8>>>);

impl Read for Chunks {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0.pop_front() {
            Some(Ok(bytes)) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Err(why)) => Err(why),
            None => Ok(0),
        }
    }
}

fn chunks(parts: &[&[u8]]) -> Chunks {
    Chunks(parts.iter().map(|part| Ok(part.to_vec())).collect())
}

/// Asserts that the request line is what comes before the first newline, whether it arrives
/// in one read or in several, and that nothing after it is read. It catches a read that waits
/// for the whole request, or one that takes only the first chunk.
#[test]
fn the_request_line_ends_at_the_first_newline_in_one_read_or_several() {
    let mut one = chunks(&[b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n"]);
    assert_eq!(request_line(&mut one), b"GET /healthz HTTP/1.1\r");
    let mut split = chunks(&[b"GET /hea", b"lthz HT", b"TP/1.1\r\n", b"more"]);
    assert_eq!(request_line(&mut split), b"GET /healthz HTTP/1.1\r");
    assert_eq!(split.0.len(), 1, "the read went on past the line");
}

/// Asserts that a line that never ends gives an empty request, whichever way it stops: the
/// client closes, a read fails, or the cap is reached with the client still sending (and the
/// read stops at the cap, leaving what comes after it). It catches an unbounded read and a
/// half line taken for a request.
#[test]
fn a_request_line_that_does_not_end_is_empty_and_the_read_is_bounded() {
    let mut closed = chunks(&[b"GET /healthz"]);
    assert!(request_line(&mut closed).is_empty());
    let mut failed = Chunks(VecDeque::from([
        Ok(b"GET /heal".to_vec()),
        Err(io::Error::other("reset")),
        Ok(b"thz HTTP/1.1\r\n".to_vec()),
    ]));
    assert!(request_line(&mut failed).is_empty());
    assert_eq!(failed.0.len(), 1, "the read went on after the failure");
    let long = vec![b'a'; REQUEST_CAP];
    let mut endless = Chunks(VecDeque::from([Ok(long), Ok(b"\n".to_vec())]));
    assert!(request_line(&mut endless).is_empty());
    assert_eq!(endless.0.len(), 1, "the read went on past the cap");
}

const PLACEABLE_ANSWER: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\n\
                                Connection: close\r\n\r\nplaceable\n";

/// Asserts the whole of the answers: `200` and `placeable` for a placeable host, `503` and the
/// reason for one that is not, and `404` for every other request line, including a method
/// other than `GET`, another path and a line with one word. It catches a status that does not
/// follow the verdict, a body without its newline or a length that is not the body's.
#[test]
fn the_route_answers_the_verdict_on_get_healthz_and_404_to_anything_else() {
    let health = admitted();
    assert_eq!(
        respond(&health, b"GET /healthz HTTP/1.1\r"),
        PLACEABLE_ANSWER
    );
    health.own_taint(true);
    let body = format!("not placeable: {OWN_TAINT}\n");
    let refused = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(respond(&health, b"GET /healthz HTTP/1.0"), refused);
    let missing = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\n\
                   Connection: close\r\n\r\nnot found\n";
    for line in [
        &b"GET /other HTTP/1.1\r"[..],
        b"POST /healthz HTTP/1.1\r",
        b"GET",
        b"",
        b"\xff\xfe /healthz",
    ] {
        assert_eq!(respond(&health, line), missing, "{line:?}");
    }
}

/// Runs `ask` against a route on a free loopback port that answers from `health`, and stops
/// the route before it returns, whether `ask` panicked or not (a panic is raised again).
fn serving<T>(health: &Health, ask: impl FnOnce(SocketAddr) -> T) -> T {
    let route = Route::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let stop = AtomicBool::new(false);
    let result = thread::scope(|scope| {
        scope.spawn(|| route.serve(health, &stop));
        let result = catch_unwind(AssertUnwindSafe(|| ask(route.addr())));
        stop.store(true, Ordering::SeqCst);
        route.wake();
        result
    });
    result.unwrap_or_else(|panic| resume_unwind(panic))
}

/// Sends `request`, closes the sending side when `close` and returns everything the route
/// answers.
fn ask(addr: SocketAddr, request: &[u8], close: bool) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request).unwrap();
    if close {
        stream.shutdown(Shutdown::Write).unwrap();
    }
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    text
}

/// Asserts the route over a socket: the status line and body that follow the health state as
/// it changes, `404` for another path, and a route that goes on answering after a request
/// that was malformed, one that stopped before its line ended and one that filled the cap with
/// no line end (each of them answered, not waited on). It catches a route that hangs on a
/// client that does not finish, and one that serves a stale verdict.
#[test]
fn the_route_serves_the_verdict_over_a_loopback_socket_and_is_not_held_by_bad_requests() {
    let health = admitted();
    let (first, other, garbage, short, full, second, last) = serving(&health, |addr| {
        let first = ask(addr, b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n", false);
        let other = ask(addr, b"GET /metrics HTTP/1.1\r\n\r\n", false);
        let garbage = ask(addr, b"\x00\x01\x02\r\n", false);
        let short = ask(addr, b"GET /healthz", true);
        let full = ask(addr, &[b'a'; REQUEST_CAP], false);
        health.server_taint(Some(residue()));
        let second = ask(addr, b"GET /healthz HTTP/1.0\r\n\r\n", false);
        health.server_taint(None);
        let last = ask(addr, b"GET /healthz HTTP/1.0\r\n\r\n", false);
        (first, other, garbage, short, full, second, last)
    });
    assert_eq!(first, PLACEABLE_ANSWER);
    for text in [&other, &garbage, &short, &full] {
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\nnot found\n"), "{text}");
    }
    assert!(second.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
    assert!(second.ends_with(&format!("\r\n\r\nnot placeable: {SERVER_RESIDUE}\n")));
    assert_eq!(last, PLACEABLE_ANSWER);
}

/// Asserts that a route is refused for an address that is not a loopback one, naming the flag,
/// and for one that is taken, naming the flag and the address, and that a loopback address
/// with port 0 takes a free port which the route names. It catches a route that listens on the
/// network, and a bind error that does not say which flag it is about.
#[test]
fn a_route_is_bound_to_a_loopback_address_only() {
    let public = Route::bind("0.0.0.0:0".parse().unwrap()).err().unwrap();
    assert!(public.starts_with("--health-listen: 0.0.0.0:0 is not a loopback address"));
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap();
    let refused = Route::bind(busy).err().unwrap();
    assert!(
        refused.starts_with(&format!("--health-listen: cannot listen on {busy}: ")),
        "{refused}"
    );
    let route = Route::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    assert!(route.addr().ip().is_loopback());
    assert_ne!(route.addr().port(), 0);
}
