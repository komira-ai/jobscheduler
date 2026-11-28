//! The real client and the real verb against a lease server in the test process: a host
//! admitted by its host certificate, a cordon placed, renewed, read and lifted over HTTPS,
//! and the refusal of a certificate that is not the host's. The server's clock is moved by the
//! test; the verb's pace is the fake one.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_agent::own_host::{CordonView, HeldLease, HttpLease, Lease, LeaseError, RequestLimit};
use js_agent::{Dialer, Login};
use js_client::{Client, ClientError};
use js_types::CordonHolder;
use serde_json::json;

use super::fakes::FakePace;
use super::verb::{Ran, drive_with};
use crate::host_certs::self_signed;
use crate::jobs::private;
use crate::server::{Agent, Server, T0};
use crate::support::{Scratch, code, linux_host, mac_host};

fn holder(name: &str) -> CordonHolder {
    CordonHolder::new(name).expect("a holder")
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A lease server, and the host `mac-1` admitted to it by its host certificate, with the state
/// root its agent uses.
pub(crate) struct Enrolled {
    pub(crate) server: Server,
    pub(crate) state: PathBuf,
    _files: Scratch,
    /// The agent of the host, held at its next wait.
    pub(crate) agent: Agent,
}

/// The lease of the host `mac-1` of `server`, presenting its host certificate.
pub(crate) fn lease_of(server: &Server) -> HttpLease {
    lease_as(server, "mac-1", "mac-1")
}

/// The lease of the host `host` of `server`, presenting the host certificate of `certified`.
pub(crate) fn lease_as(server: &Server, certified: &str, host: &str) -> HttpLease {
    let identities = server.identities();
    HttpLease::connect(
        &server.url(),
        &Login::HostCertificate(identities.of(certified)),
        Some(identities.server_cert()),
        host,
        RequestLimit::DEFAULT,
    )
    .unwrap()
}

impl Enrolled {
    /// The client of the host, with its own certificate.
    fn client(&self) -> HttpLease {
        lease_of(&self.server)
    }
}

/// Starts a server and enrols `mac-1` on it: the agent's hello, made with the host's
/// certificate, is answered with a welcome.
pub(crate) fn enrol(name: &str) -> Enrolled {
    let server = Server::start(name, &[]);
    let files = Scratch::new(&format!("{name}-files"));
    let state = private(&files, "state");
    let state_flag = state.display().to_string();
    let flags = ["--host-id", "mac-1", "--state-root", &state_flag];
    let agent = Agent::spawn(server.url(), server.identities(), &flags, mac_host, Dialer);
    assert_eq!(agent.next_wait(), secs(5), "the hello");
    Enrolled {
        server,
        state,
        _files: files,
        agent,
    }
}

/// The verb's flags that make it the host `mac-1` of `server`: the server, the host's
/// certificate and key and the certificate to trust.
pub(crate) fn as_host(server: &Server) -> Vec<String> {
    let mut flags = vec!["--server".to_owned(), server.url()];
    flags.extend(server.identities().flags("mac-1"));
    flags.extend(["--host-id".to_owned(), "mac-1".to_owned()]);
    flags
}

/// The connector of the program: a real client for the host.
pub(super) fn real(
    at: &js_agent::cordon::Connection<'_>,
) -> Result<Box<dyn HeldLease>, ClientError> {
    HttpLease::connect(
        at.server,
        at.login,
        at.ca_cert,
        at.host,
        RequestLimit::DEFAULT,
    )
    .map(|lease| Box::new(lease) as Box<dyn HeldLease>)
}

/// The own-host routes of `mac-1` at the plain-HTTP server `url` of the test, a server of its
/// own that answers what the test wants it to, with the bearer in `token`: the client's requests
/// and reading of answers, which a real server cannot be made to give wrongly.
fn plain_lease(url: &str, token: &std::path::Path) -> HttpLease {
    let client = Client::connect(url, token, None, RequestLimit::DEFAULT.get()).unwrap();
    HttpLease::over(client, "mac-1")
}

/// Runs the verb on `flags` (and the host certificate flags it requires, whose files that
/// connector never reads) against the plain-HTTP server of the test, over a client with the
/// bearer in `token`.
fn plain_verb(flags: &[&str], token: &std::path::Path) -> Ran {
    let token = token.to_owned();
    let connect =
        move |at: &js_agent::cordon::Connection<'_>| -> Result<Box<dyn HeldLease>, ClientError> {
            let client = Client::connect(at.server, &token, None, RequestLimit::DEFAULT.get())?;
            Ok(Box::new(HttpLease::over(client, at.host)) as Box<dyn HeldLease>)
        };
    let mut flags = flags.to_vec();
    flags.extend([
        "--host-cert",
        "/never/read.crt",
        "--host-key",
        "/never/read.key",
    ]);
    drive_with(&flags, &linux_host(), &connect, &mut FakePace::new())
}

/// Runs the verb for real on `flags`, over a pace the test gave.
fn verb(flags: &[&str], pace: &mut FakePace<'_>) -> Ran {
    drive_with(flags, &linux_host(), &real, pace)
}

/// Asserts what the client does on the wire with the host's own certificate: a read shows a
/// live host with no cordon and no mark; a cordon of 120 s says it was not cordoned before,
/// carries the server's expiry, which the server's host listing shows too; a second cordon
/// of a shorter lease says it was cordoned already and does not shorten it; an uncordon says
/// it lifted one and the listing shows none; a second uncordon says nothing was lifted. That
/// a cordoned host is placed no job is asserted with a container agent in `jobs/e2e.rs`. It
/// catches a client that sends the wrong
/// path, drops `already_cordoned` or `lifted`, or reads the expiry wrong.
#[test]
fn the_client_cordons_reads_and_uncordons_a_real_host_with_its_certificate() {
    let enrolled = enrol("cordon_http_client");
    let (server, lease) = (&enrolled.server, enrolled.client());

    let before = lease.host().unwrap();
    assert_eq!(
        (before.id.as_str(), before.state.as_str(), before.leases),
        ("mac-1", "live", 0)
    );
    assert!(before.lease.is_none() && before.mark.is_none() && before.cordon.is_none());
    assert!(!before.releasing);

    let placed = lease.cordon(120).unwrap();
    assert!(!placed.already_cordoned);
    let expiry = CordonView {
        expires_at_ms: T0 + 120_000,
        expires_in_ms: Some(120_000),
        holder: None,
    };
    assert_eq!(placed.host.cordon.as_ref(), Some(&expiry));
    assert_eq!(
        server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 120_000
    );

    let again = lease.cordon(60).unwrap();
    assert!(again.already_cordoned);
    assert_eq!(
        again.host.cordon.as_ref(),
        Some(&expiry),
        "a repeat never shortens"
    );
    assert_eq!(lease.host().unwrap().cordon, Some(expiry));

    let lifted = lease.uncordon().unwrap();
    assert!(lifted.lifted);
    assert!(lifted.host.cordon.is_none());
    assert!(server.host("mac-1").get("cordon").is_none());
    assert!(lease.host().unwrap().cordon.is_none());

    assert!(!lease.uncordon().unwrap().lifted);
}

/// Asserts the refusal of a certificate that is not the host's, as the client reports them: the
/// host certificate of `mac-2` presented for the routes of `mac-1` is refused 403
/// `certificate-host-mismatch` on all three, and the verb with the same flags ends with status 1,
/// calling it a credential error and naming the refusal. It catches a client that reports a refused certificate as an
/// unreachable server, and a server that lets one host act for another.
#[test]
fn a_certificate_of_another_host_is_refused_on_all_three_routes() {
    let enrolled = enrol("cordon_http_refused");
    let server = &enrolled.server;
    let stranger = lease_as(server, "mac-2", "mac-1");
    let denied = || LeaseError::Refused("certificate-host-mismatch".to_owned());
    assert_eq!(stranger.cordon(60).unwrap_err(), denied());
    assert_eq!(stranger.uncordon().unwrap_err(), denied());
    assert_eq!(stranger.host().unwrap_err(), denied());
    assert!(server.host("mac-1").get("cordon").is_none());

    let mut flags = vec!["--server".to_owned(), server.url()];
    flags.extend(server.identities().flags("mac-2"));
    flags.extend(["--host-id", "mac-1", "--for", "1h"].map(str::to_owned));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(1), "{}", ran.err);
    assert!(
        ran.err.starts_with(
            "js-agent cordon: credential error: cannot cordon the host: the server refused the \
             host certificate (certificate-host-mismatch)\n"
        ),
        "{}",
        ran.err
    );
    assert!(ran.out.is_empty());
}

/// Asserts what a restarted server says to a host that said hello before the restart: the
/// server holds the host's session in its row (the job tables persist), so with no hello since
/// the host's own certificate places a cordon on it (live, epoch 1, the session it had), the
/// read then shows the cordon, the lift is served and the verb ends with status 0. It catches a restarted
/// server that answers a host it can read from its rows as unknown, which would make the
/// operator's cordon fail until the agent's next hello.
#[test]
fn a_restarted_server_serves_the_host_its_rows_hold() {
    let Enrolled {
        server,
        state: _state,
        _files,
        agent: _agent,
    } = enrol("cordon_http_restarted");
    let server = server.restart();
    let lease = lease_of(&server);
    let placed = lease.cordon(60).unwrap();
    assert!(!placed.already_cordoned);
    assert_eq!((placed.host.state.as_str(), placed.host.epoch), ("live", 1));
    assert!(lease.host().unwrap().cordon.is_some());
    lease.uncordon().unwrap();

    let mut flags = as_host(&server);
    flags.extend(["--for".to_owned(), "1h".to_owned()]);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(0), "{}", ran.err);
}

/// Asserts what a server whose job tables hold no row for the host says to it: the certificate
/// is still the host's and the tables hold no such host, so the client reports `UnknownHost`
/// and the verb ends with status 1 saying the server holds no such host. It catches an unknown
/// host reported as a refused certificate, which would send an operator to the wrong place.
#[cfg(target_os = "linux")]
#[test]
fn a_server_whose_tables_hold_no_row_for_the_host_says_unknown_host() {
    let Enrolled {
        server,
        state: _state,
        _files,
        agent: _agent,
    } = enrol("cordon_http_unknown");
    let server = server.restart_forgetting_jobs();
    let lease = lease_of(&server);
    assert_eq!(lease.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.host().unwrap_err(), LeaseError::UnknownHost);

    let mut flags = as_host(&server);
    flags.extend(["--for".to_owned(), "1h".to_owned()]);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(1));
    assert!(
        ran.err.starts_with(
            "js-agent cordon: server error: cannot cordon the host: the server holds no such host"
        ),
        "{}",
        ran.err
    );
}

/// Serves `times` connections with a 200 answer whose body is not a host reading, after
/// reading each request to its end.
fn serve_garbage(times: usize) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for stream in listener.incoming().take(times) {
            let mut stream = stream.unwrap();
            stream.set_read_timeout(Some(secs(60))).unwrap();
            let mut seen = Vec::new();
            while !request_complete(&seen) {
                let mut chunk = [0_u8; 1024];
                let n = stream.read(&mut chunk).unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            let body = r#"{"unexpected":true}"#;
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(answer.as_bytes()).unwrap();
        }
    });
    (url, handle)
}

/// Whether `seen` holds a request head and as many bytes of body as it declares.
fn request_complete(seen: &[u8]) -> bool {
    let text = String::from_utf8_lossy(seen);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let declared = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .find_map(|value| value.trim().parse::<usize>().ok());
    body.len() >= declared.unwrap_or(0)
}

/// Asserts that an answer which is not what the route answers (a 200 with another body) is
/// reported as a server that was not understood, on each of the three routes, and ends the verb
/// with status 1. It catches a client that panics or invents a host from a body it could not
/// read.
#[test]
fn an_answer_that_is_not_a_host_reading_is_not_understood() {
    let (url, served) = serve_garbage(6);
    let files = Scratch::new("cordon_http_garbage");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    let lease = plain_lease(&url, &token);
    for error in [
        lease.cordon(60).unwrap_err(),
        lease.uncordon().unwrap_err(),
        lease.host().unwrap_err(),
        lease.cordon_held(60, &holder("me"), false).unwrap_err(),
        lease.uncordon_held(&holder("me")).unwrap_err(),
    ] {
        let LeaseError::Unreachable(text) = error else {
            panic!("{error:?}");
        };
        assert!(text.starts_with("the answer is not understood: "), "{text}");
    }
    let flags = ["--server", &url, "--host-id", "mac-1", "--for", "1h"];
    let ran = plain_verb(&flags, &token);
    assert_eq!(ran.status, code(1));
    assert!(
        ran.err
            .contains("server error: cannot cordon the host: the answer is not understood")
    );
    served.join().unwrap();
}

/// Asserts that a server nothing listens on is a server error with status 1, naming the
/// address and not the key. It catches an unreachable server reported as a usage error
/// (status 2) or as a refusal.
#[test]
fn a_server_that_cannot_be_reached_is_a_server_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    drop(listener);
    let files = Scratch::new("cordon_http_closed");
    let identity = self_signed(&files, "mac-1");
    let (cert, key) = (
        identity.cert_path().display().to_string(),
        identity.key_path().display().to_string(),
    );
    let flags = [
        "--server",
        &url,
        "--host-cert",
        &cert,
        "--host-key",
        &key,
        "--host-id",
        "mac-1",
        "--for",
        "1h",
    ];
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(1));
    let start =
        format!("js-agent cordon: server error: cannot cordon the host: cannot talk to {url}");
    assert!(ran.err.starts_with(&start), "{}", ran.err);
    let pem = std::fs::read_to_string(&key).unwrap();
    assert!(
        !ran.err.contains(pem.lines().nth(1).unwrap()),
        "{}",
        ran.err
    );
}

/// Asserts the whole verb against a real server over a window of 100 s with cordons of 30 s,
/// while the server's clock moves with the verb's: the cordon is on the server after every
/// wait and never past its expiry (so it is renewed, not just placed), the lines are
/// `cordoned`, `idle`, `lifted`, and the server shows no cordon afterwards. It catches a verb whose renewals do not reach the server, and one that does not lift
/// what it placed.
#[test]
fn the_verb_holds_a_real_cordon_for_the_window_and_lifts_it() {
    let enrolled = enrol("cordon_http_window");
    let server = &enrolled.server;
    let elapsed = Cell::new(0_u64);
    let checks = Cell::new(0_u32);
    let mut pace = FakePace::new().on_wait(|waits, span| {
        server.clock.advance(span);
        elapsed.set(elapsed.get() + u64::try_from(span.as_millis()).unwrap());
        let cordon = server.host("mac-1")["cordon"].clone();
        let expires = cordon["expires_at_ms"]
            .as_u64()
            .expect("the cordon is on the server");
        assert!(expires > T0 + elapsed.get(), "wait {waits}: {cordon}");
        checks.set(checks.get() + 1);
    });
    let mut flags = as_host(server);
    flags.extend(["--for", "100s", "--lease-secs", "30", "--holder", "ops-1"].map(str::to_owned));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = verb(&flags, &mut pace);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(
        ran.out,
        "cordoned host=mac-1 holder=ops-1 expires_in_ms=30000 leases=0\nidle host=mac-1\nlifted host=mac-1\n"
    );
    assert_eq!(checks.get(), 10, "a renewal every 10 s of 100");
    assert!(server.host("mac-1").get("cordon").is_none());
}

/// Asserts that a cordon someone else placed is left alone, whoever placed it, against the real
/// server: an unnamed requester's cordon (an operator's) and a named holder's each make the
/// verb exit with status 3 printing `kept` with the holder (`-` for none) and the time left (within the 600 s the cordon was placed for),
/// and the server still shows the cordon, with its expiry unchanged (a found cordon is not
/// extended) and its holder. It catches an uncordon sent for a cordon the verb did not place,
/// and a found cordon that is extended.
#[test]
fn another_holders_cordon_on_a_real_server_is_kept() {
    let held: [(Option<&str>, &str); 2] = [(None, "-"), (Some("alice"), "alice")];
    for (named, shown) in held {
        let enrolled = enrol("cordon_http_other");
        let other = enrolled.client();
        let placed = match named {
            Some(name) => other.cordon_held(600, &holder(name), false).unwrap(),
            None => other.cordon(600).unwrap(),
        };
        assert!(!placed.already_cordoned);
        let mut flags = as_host(&enrolled.server);
        flags.extend(["--for", "1h", "--holder", "ops-1"].map(str::to_owned));
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        let before = enrolled.server.host("mac-1")["cordon"]["expires_at_ms"].clone();
        let mut pace = FakePace::new();
        let ran = verb(&flags, &mut pace);
        assert_eq!(ran.status, code(3), "{}", ran.err);
        let head = format!("kept host=mac-1 reason=already_cordoned holder={shown} expires_in_ms=");
        let left = ran.out.strip_prefix(&head).expect(&ran.out);
        let left: u64 = left.trim_end().parse().expect(&ran.out);
        assert!((1..=600_000).contains(&left), "{left}");
        assert!(pace.waits.is_empty());
        let cordon = enrolled.server.host("mac-1")["cordon"].clone();
        assert!(before.is_u64(), "{before}");
        assert_eq!(
            cordon["expires_at_ms"], before,
            "a found cordon is not extended"
        );
        assert_eq!(cordon["holder"], json!(named));
    }
}

/// Serves one connection per canned `reply` (the body of a 200 answer), in order, and returns
/// the request line and body of each request it read.
fn serve_recording(replies: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (body, stream) in replies.into_iter().zip(listener.incoming()) {
            let mut stream = stream.unwrap();
            stream.set_read_timeout(Some(secs(60))).unwrap();
            let mut seen = Vec::new();
            while !request_complete(&seen) {
                let mut chunk = [0_u8; 1024];
                let n = stream.read(&mut chunk).unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8(seen).unwrap();
            let (head, sent) = text.split_once("\r\n\r\n").unwrap();
            let line = head.lines().next().unwrap();
            requests.push(format!("{line} {sent}"));
            let answer = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(answer.as_bytes()).unwrap();
        }
        requests
    });
    (url, handle)
}

/// Asserts what the client sends and reads for a named holder: the cordon carries
/// `lease_secs` and `holder`, and `take_over: true` only when a take-over is asked (the key is
/// absent otherwise); the uncordon carries `holder` alone; the replies' `held_by_other` and
/// the cordon's `holder` are read, and a reply of an earlier server that has neither reads as
/// not held by another and holder-less. It catches a body with a wrong key, a take-over sent
/// when it was not asked, a field of the reply that is dropped, and a reply without the new
/// fields that is refused.
#[test]
fn the_client_names_the_holder_and_reads_whose_the_cordon_is() {
    let reading = |extra: &str, cordon: &str| {
        format!(
            r#"{{"id":"mac-1","state":"live","epoch":1,"leases":0,"releasing":false,{extra}{cordon}}}"#
        )
    };
    let held = r#""cordon":{"expires_at_ms":9000,"expires_in_ms":600,"holder":"alice"}"#;
    let bare = r#""cordon":{"expires_at_ms":9000}"#;
    let replies = vec![
        reading(r#""already_cordoned":true,"held_by_other":true,"#, held),
        reading(r#""already_cordoned":true,"held_by_other":false,"#, held),
        reading(r#""already_cordoned":false,"#, bare),
        reading(r#""lifted":false,"held_by_other":true,"#, held),
        reading(r#""lifted":true,"#, r#""agent_version":"1""#),
    ];
    let (url, served) = serve_recording(replies);
    let files = Scratch::new("cordon_http_holder");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    let lease = plain_lease(&url, &token);
    let me = holder("me");

    let kept = lease.cordon_held(90, &me, false).unwrap();
    assert!(kept.already_cordoned && kept.held_by_other);
    let view = kept.host.cordon.expect("the cordon");
    assert_eq!(view.holder.as_deref(), Some("alice"));
    assert_eq!(view.expires_in_ms, Some(600));

    let taken = lease.cordon_held(90, &me, true).unwrap();
    assert!(taken.already_cordoned && !taken.held_by_other);

    let earlier = lease.cordon_held(30, &me, false).unwrap();
    assert!(!earlier.already_cordoned && !earlier.held_by_other);
    assert_eq!(earlier.host.cordon.expect("the cordon").holder, None);

    let left = lease.uncordon_held(&me).unwrap();
    assert!(!left.lifted && left.held_by_other);
    let lifted = lease.uncordon_held(&me).unwrap();
    assert!(lifted.lifted && !lifted.held_by_other);

    let requests = served.join().unwrap();
    let post = |verb: &str, body: &str| format!("POST /v1/hosts/mac-1:{verb} HTTP/1.1 {body}");
    let bodies: Vec<_> = requests.iter().map(String::as_str).collect();
    assert_eq!(bodies.len(), 5, "{bodies:?}");
    let sent = |at: usize| {
        let (line, body) = requests[at].split_once(" {").expect("a body");
        (
            line.to_owned(),
            serde_json::from_str::<serde_json::Value>(&format!("{{{body}")).unwrap(),
        )
    };
    let cordon = post("cordon", "").trim_end().to_owned();
    let uncordon = post("uncordon", "").trim_end().to_owned();
    assert_eq!(
        sent(0),
        (cordon.clone(), json!({"lease_secs": 90, "holder": "me"}))
    );
    assert_eq!(
        sent(1),
        (
            cordon.clone(),
            json!({"lease_secs": 90, "holder": "me", "take_over": true})
        )
    );
    assert_eq!(sent(2), (cordon, json!({"lease_secs": 30, "holder": "me"})));
    assert_eq!(sent(3), (uncordon.clone(), json!({"holder": "me"})));
    assert_eq!(sent(4), (uncordon, json!({"holder": "me"})));
}
