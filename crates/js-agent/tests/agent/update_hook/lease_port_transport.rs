//! The lease port of the hook on a machine ([`MachineLease`]) when the transport fails or the
//! server answers what is not a reading: the request limit it is given and its range, a server
//! that is down, one that stalls before the head, in the middle of the body or in the TLS
//! handshake, a call answered after a timeout on the same port, the answers that are and are not
//! the typed unknown host, and an older server's reading. The steps that loop on the server run
//! over the fakes' clock: their bound is the step's, and no test waits for it or measures it.

use std::net::TcpStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use js_agent::own_host::{
    HostReading, Lease, LeaseError, MAX_REQUEST_LIMIT, RequestLimit, RequestLimitOutOfRange,
};
use js_agent::update_hook::{Class, Outcome, POLL_SECS, Reason, step};

use super::lease_port::{all_unreachable, cut_over, lease, reach, short, unreachable, with_lease};
use super::lease_stub::{Reply, Stub, closed};
use super::step_rig::Rig;
use crate::cordon::http::enrol;
use crate::support::Scratch;

/// A host reading of every field, as a current server sends it.
const READING: &str = r#"{"id":"mac-1","state":"live","epoch":3,"session_started_ms":5000,"leases":0,"releasing":false,"agent_version":"1.4.0","runtime":"crun"}"#;

/// A host reading with only the fields every server has sent.
const OLDER: &str = r#"{"id":"mac-1","state":"awaiting","epoch":0,"leases":0,"releasing":false}"#;

/// [`READING`], read.
fn reading() -> HostReading {
    HostReading {
        id: "mac-1".to_owned(),
        state: "live".to_owned(),
        epoch: 3,
        session_started_ms: Some(5000),
        leases: 0,
        lease: None,
        releasing: false,
        mark: None,
        cordon: None,
        taint: None,
        agent_version: Some("1.4.0".to_owned()),
        runtime: Some("crun".to_owned()),
    }
}

/// A token file with a token in it: the stub servers take any bearer.
fn token(files: &Scratch) -> std::path::PathBuf {
    files.write("token", "token-0123456789abcdef", 0o600)
}

/// What a call that ran out of its limit says.
fn timed_out(url: &str) -> String {
    format!("cannot talk to {url}: timeout: global")
}

/// Asserts that the limit the port is given is the one each call is held to, against a live
/// server that answers well within the default: with a limit of 1 ns the read, the cordon and
/// the uncordon each time out and the server shows no cordon; with the default limit the same
/// params read the host. No time is measured: 1 ns is over before any request can be answered.
/// It catches a port that ignores the limit it is given for a default of its own.
#[test]
fn the_limit_the_port_is_given_is_the_one_every_call_is_held_to() {
    let enrolled = enrol("hook_lease_limit_live");
    let url = enrolled.server.url();
    let params = reach(&url, Some(enrolled.state.clone()), None);
    let instant = RequestLimit::new(Duration::from_nanos(1)).unwrap();
    for text in all_unreachable(&lease(&params, instant)) {
        assert_eq!(text, timed_out(&url));
    }
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
    assert_eq!(
        lease(&params, RequestLimit::DEFAULT).host().unwrap().id,
        "mac-1"
    );
}

/// Asserts the range of a request limit: zero, a nanosecond past the maximum, `u64::MAX`
/// seconds and `Duration::MAX` are each refused by `RequestLimitOutOfRange` with the value given,
/// in words that name the range; 1 ns, 1 ms and the maximum are kept as given, and the default
/// is the agent's own limit; and a port made with the maximum is answered (its deadline does
/// not overflow). It catches a limit that reaches the client unchecked: a huge one panics in
/// the client's deadline arithmetic, and zero fails every call unsent.
#[test]
fn a_request_limit_of_zero_or_past_the_maximum_is_refused_by_name() {
    for bad in [
        Duration::ZERO,
        MAX_REQUEST_LIMIT + Duration::from_nanos(1),
        Duration::from_secs(u64::MAX),
        Duration::MAX,
    ] {
        assert_eq!(RequestLimit::new(bad), Err(RequestLimitOutOfRange(bad)));
    }
    assert_eq!(
        RequestLimitOutOfRange(Duration::ZERO).to_string(),
        "a request limit must be more than zero and at most 3600s, not 0ns"
    );
    for good in [
        Duration::from_nanos(1),
        Duration::from_millis(1),
        MAX_REQUEST_LIMIT,
    ] {
        assert_eq!(RequestLimit::new(good).unwrap().get(), good);
    }
    assert_eq!(RequestLimit::DEFAULT.get(), js_agent::REQUEST_LIMIT);

    let files = Scratch::new("hook-lease-limit-range");
    let stub = Stub::start(&[Reply::Answer(200, READING)]);
    let longest = RequestLimit::new(MAX_REQUEST_LIMIT).unwrap();
    let port = lease(&reach(&stub.url, None, Some(token(&files))), longest);
    assert_eq!(port.host().unwrap(), reading());
    assert_eq!(stub.stop(), 1);
}

/// Asserts that a stall at each point of a call ends it at the limit as an unreachable lease,
/// and that the same port is answered on its next call: no head, then a body cut short after
/// its head declared more, each a timeout, then a reading; and over `https://`, a server that
/// takes the connection and never sends its hello times out each of the three calls. Each call
/// is one connection. It catches a limit that covers only part of a call (the head but not the
/// body, or not the handshake), and a port left unusable after a timeout.
#[test]
fn a_stall_at_any_point_of_a_call_is_a_timeout_and_the_same_port_is_answered_after_it() {
    let files = Scratch::new("hook-lease-stalls");
    let stub = Stub::start(&[Reply::Silent, Reply::Partial, Reply::Answer(200, READING)]);
    let port = lease(&reach(&stub.url, None, Some(token(&files))), short());
    assert_eq!(unreachable(port.host()), timed_out(&stub.url), "no head");
    assert_eq!(
        unreachable(port.host()),
        timed_out(&stub.url),
        "a short body"
    );
    assert_eq!(
        port.host().unwrap(),
        reading(),
        "answered after the timeouts"
    );
    assert_eq!(stub.stop(), 3);

    let tls = Stub::start(&[Reply::Silent]);
    let port = lease(&reach(&tls.https_url(), None, Some(token(&files))), short());
    for text in all_unreachable(&port) {
        assert_eq!(text, timed_out(&tls.https_url()));
    }
    assert_eq!(tls.stop(), 3);
}

/// Asserts that a server nothing listens on and one that takes the connection and never
/// answers both end each call of the port as an unreachable lease, the second as a timeout;
/// that `prepare` over either refuses with `lease_unreachable`, class environment, and writes no
/// record; and that `verify` over either looks once per poll and ends at its own bound on the
/// fakes' clock: 30 s of bound is three pauses and four looks. The silent server counts every
/// connection, so the three calls, the one look of `prepare` and the four of `verify` each
/// reached it. It catches a port with no request limit (a step would hang on a silent server
/// past the updater's timeout), a failure taken as an answer, and a step whose wait is not
/// bounded by its clock.
#[test]
fn a_server_that_is_down_or_silent_is_a_bounded_unreachable_lease_and_the_steps_end_on_it() {
    let bound = 3 * POLL_SECS;
    let (_held, down) = closed();
    let silent = Stub::start(&[Reply::Silent]);
    let files = Scratch::new("hook-lease-port-down");
    for (url, timeout) in [(down.as_str(), false), (silent.url.as_str(), true)] {
        let port = lease(&reach(url, None, Some(token(&files))), short());
        for text in all_unreachable(&port) {
            assert!(
                text.starts_with(&format!("cannot talk to {url}: ")),
                "{text}"
            );
            assert_eq!(text == timed_out(url), timeout, "{text}");
        }

        let rig = Rig::new("hook-lease-port-down-prepare");
        let done = with_lease(&rig, &port, step::prepare);
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
        assert_eq!(done.report.class, Class::Environment);
        assert!(rig.record().is_none(), "nothing was written");

        let rig = cut_over("hook-lease-port-down-verify", bound);
        let ticks = rig.world.ticks();
        let done = with_lease(&rig, &port, step::verify);
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
        assert_eq!(done.report.class, Class::Environment);
        let detail = done.report.detail.unwrap();
        assert!(
            detail.starts_with(&format!("cannot talk to {url}: ")),
            "{detail}"
        );
        assert_eq!(
            rig.world.ticks() - ticks,
            3,
            "one pause per look until the bound"
        );
        assert!(!rig.record().unwrap().verify_ok);
    }
    assert_eq!(silent.stop(), 3 + 1 + 4);
}

/// Asserts how each refusal is told on each of the three calls: only a 409 whose code is
/// `unknown-host` is the typed `UnknownHost`; another 409 code and `unknown-host` under another
/// status are refusals by their code; a code that is not a string, an empty body and a body
/// that is not JSON are refusals by `http-<status>`; and a 200 whose body is an error is not
/// understood. It catches a port that takes every 409 for an unknown host, ignores the status,
/// or invents a code from a body that has none.
#[test]
fn only_a_409_unknown_host_is_the_unknown_host_and_every_other_refusal_keeps_its_code() {
    let files = Scratch::new("hook-lease-refusals");
    let refused = |code: &str| LeaseError::Refused(code.to_owned());
    for (status, body, want) in [
        (409, r#"{"error":"unknown-host"}"#, LeaseError::UnknownHost),
        (409, r#"{"error":"host-live"}"#, refused("host-live")),
        (404, r#"{"error":"unknown-host"}"#, refused("unknown-host")),
        (
            403,
            r#"{"error":"host-credential-required"}"#,
            refused("host-credential-required"),
        ),
        (409, r#"{"error":5}"#, refused("http-409")),
        (409, "", refused("http-409")),
        (500, "not json", refused("http-500")),
    ] {
        let stub = Stub::start(&[Reply::Answer(status, body)]);
        let port = lease(&reach(&stub.url, None, Some(token(&files))), short());
        assert_eq!(port.host().unwrap_err(), want, "{status} {body}");
        assert_eq!(port.cordon(60).unwrap_err(), want, "{status} {body}");
        assert_eq!(port.uncordon().unwrap_err(), want, "{status} {body}");
        assert_eq!(stub.stop(), 3);
    }
    let stub = Stub::start(&[Reply::Answer(200, r#"{"error":"unknown-host"}"#)]);
    let port = lease(&reach(&stub.url, None, Some(token(&files))), short());
    for text in all_unreachable(&port) {
        assert!(text.starts_with("the answer is not understood: "), "{text}");
    }
    assert_eq!(stub.stop(), 3);
}

/// Asserts that a reading from a server that sends only the fields every server has sent (a
/// host still awaiting its hello: no session, mark, cordon, taint, version or runtime) is read
/// with each of those absent, not refused. It catches a port that requires a field an older
/// server leaves out.
#[test]
fn an_older_servers_reading_of_an_awaiting_host_is_read_with_its_absent_fields() {
    let files = Scratch::new("hook-lease-older");
    let stub = Stub::start(&[Reply::Answer(200, OLDER)]);
    let port = lease(&reach(&stub.url, None, Some(token(&files))), short());
    assert_eq!(
        port.host().unwrap(),
        HostReading {
            id: "mac-1".to_owned(),
            state: "awaiting".to_owned(),
            epoch: 0,
            session_started_ms: None,
            leases: 0,
            lease: None,
            releasing: false,
            mark: None,
            cordon: None,
            taint: None,
            agent_version: None,
            runtime: None,
        }
    );
    assert_eq!(stub.stop(), 1);
}

/// Asserts that a stub dropped by a failing assertion, while it holds a connection, shuts down
/// all the same: once the panic has unwound, its accept thread has ended, and with it the
/// listener and the connection it held. It catches a stand-in that would keep a thread and its
/// sockets for the rest of the test process after a failure.
#[test]
fn a_stub_dropped_by_a_failed_assertion_stops_its_thread_and_sockets() {
    let mut alive = None;
    let failed = catch_unwind(AssertUnwindSafe(|| {
        let stub = Stub::start(&[Reply::Silent]);
        alive = Some(stub.alive());
        let _held = TcpStream::connect(stub.url.trim_start_matches("http://")).unwrap();
        assert_eq!(1, 2, "an assertion that fails while the stub runs");
    }));
    assert!(failed.is_err());
    let alive = alive.unwrap();
    assert!(
        alive.upgrade().is_none(),
        "the thread and its sockets are gone"
    );
}
