//! The real client and the real verb against a lease server in the test process: a host
//! admitted with a one-time token and given a host credential by an agent, a cordon placed,
//! renewed, read and lifted over HTTP, and the refusals of a credential that is not the
//! host's. The server's clock is moved by the test; the verb's pace is the fake one.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_agent::own_host::{CordonView, HttpLease, Lease, LeaseError};
use js_agent::{CREDENTIAL_FILE, Dialer};
use js_client::ClientError;
use serde_json::json;

use super::fakes::FakePace;
use super::verb::{Ran, drive_with};
use crate::jobs::private;
use crate::server::{Agent, JOIN, Server, T0};
use crate::support::{Host, Scratch, code, linux_host, mac_host};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A Mac whose instance file says it runs on `lambdalabs:i-1`.
fn launched() -> Host {
    let mut host = mac_host();
    host.files = host.files.with("/instance", "lambdalabs:i-1\n");
    host
}

/// A lease server, and the host `mac-1` admitted to it with a one-time token that its agent
/// swapped for a host credential kept in `state`.
pub(crate) struct Enrolled {
    pub(crate) server: Server,
    pub(crate) state: PathBuf,
    _files: Scratch,
    _agent: Agent,
}

impl Enrolled {
    /// The state root as a flag value.
    pub(super) fn state_flag(&self) -> String {
        self.state.display().to_string()
    }

    /// The client of the host, with its own credential.
    fn client(&self) -> HttpLease {
        let credential = self.state.join(CREDENTIAL_FILE);
        HttpLease::connect(&self.server.url(), &credential, None, "mac-1").unwrap()
    }
}

/// Starts a server and enrols `mac-1` on it: the agent's hello is answered with a host
/// credential, which it saves in the state root before its next call.
pub(crate) fn enrol(name: &str) -> Enrolled {
    let server = Server::start(name, &[]);
    let files = Scratch::new(&format!("{name}-files"));
    let instance = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    let minted = server.post_body("/v1/agent/join-tokens", &instance);
    assert_eq!(minted.status, 201, "{}", minted.body);
    let token = files.write(
        "one-time-token",
        minted.body["token"].as_str().unwrap(),
        0o600,
    );
    let state = private(&files, "state");
    let state_flag = state.display().to_string();
    let flags = [
        "--host-id",
        "mac-1",
        "--instance-file",
        "/instance",
        "--state-root",
        &state_flag,
    ];
    let agent = Agent::spawn(server.url(), token, &flags, launched, Dialer);
    assert_eq!(agent.next_wait(), secs(5), "the hello");
    assert!(state.join(CREDENTIAL_FILE).exists());
    Enrolled {
        server,
        state,
        _files: files,
        _agent: agent,
    }
}

/// The connector of the program: a real client for the host.
fn real(at: &js_agent::cordon::Connection<'_>) -> Result<Box<dyn Lease>, ClientError> {
    HttpLease::connect(at.server, at.credential, at.ca_cert, at.host)
        .map(|lease| Box::new(lease) as Box<dyn Lease>)
}

/// Runs the verb for real on `flags`, over a pace the test gave.
fn verb(flags: &[&str], pace: &mut FakePace<'_>) -> Ran {
    drive_with(flags, &linux_host(), &real, pace)
}

/// Asserts what the client does on the wire with the host's own credential: a read shows a
/// live host with no cordon and no mark; a cordon of 120 s says it was not cordoned before,
/// carries the server's expiry, and keeps the server from placing a lease (409 `no-capacity`);
/// a second cordon of a shorter lease says it was cordoned already and does not shorten it; an
/// uncordon says it lifted one and the server places a lease again, which the next read
/// shows; a second uncordon says nothing was lifted. It catches a client that sends the wrong
/// path, drops `already_cordoned` or `lifted`, or reads the expiry wrong.
#[test]
fn the_client_cordons_reads_and_uncordons_a_real_host_with_its_credential() {
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
    };
    assert_eq!(placed.host.cordon, Some(expiry));
    assert_eq!(
        server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 120_000
    );
    let refused = server.lease();
    assert_eq!(
        (refused.status, refused.body),
        (409, json!({ "error": "no-capacity" }))
    );

    let again = lease.cordon(60).unwrap();
    assert!(again.already_cordoned);
    assert_eq!(again.host.cordon, Some(expiry), "a repeat never shortens");
    assert_eq!(lease.host().unwrap().cordon, Some(expiry));

    let lifted = lease.uncordon().unwrap();
    assert!(lifted.lifted);
    assert!(lifted.host.cordon.is_none());
    assert!(server.host("mac-1").get("cordon").is_none());
    let granted = server.lease();
    assert_eq!(granted.status, 201, "{}", granted.body);
    let busy = lease.host().unwrap();
    assert_eq!(busy.leases, 1);
    assert_eq!(busy.lease.as_deref(), granted.body["id"].as_str());

    assert!(!lease.uncordon().unwrap().lifted);
}

/// Asserts the refusals of a credential that is not the host's, as the client reports them:
/// the shared pool token is refused 403 `host-credential-required` on all three routes, and a
/// token the server does not know is a 401 with no body, which the client names `http-401`;
/// the verb ends with status 1 and calls both a credential error, and neither the token nor
/// anything of it is on standard output or error. It catches a client that reports a refused
/// credential as an unreachable server, and a verb that prints the secret it was given.
#[test]
fn the_pool_token_and_an_unknown_token_are_refused_as_credential_errors() {
    let server = Server::start("cordon_http_refused", &[]);
    let files = Scratch::new("cordon_http_refused-files");
    let unknown = "an-unknown-token-0123456789abcdef";
    let unknown_file = files.write("unknown", unknown, 0o600);
    let url = server.url();

    let pooled = HttpLease::connect(&url, &server.join_file(), None, "mac-1").unwrap();
    let denied = || LeaseError::Refused("host-credential-required".to_owned());
    assert_eq!(pooled.cordon(60).unwrap_err(), denied());
    assert_eq!(pooled.uncordon().unwrap_err(), denied());
    assert_eq!(pooled.host().unwrap_err(), denied());
    let stranger = HttpLease::connect(&url, &unknown_file, None, "mac-1").unwrap();
    assert_eq!(
        stranger.cordon(60).unwrap_err(),
        LeaseError::Refused("http-401".to_owned())
    );

    for (file, token, why) in [
        (server.join_file(), JOIN, "host-credential-required"),
        (unknown_file, unknown, "http-401"),
    ] {
        let flags = ["--server", &url, "--host-id", "mac-1", "--for", "1h"];
        let mut flags = flags.to_vec();
        let file = file.display().to_string();
        flags.extend(["--token-file", &file]);
        let ran = verb(&flags, &mut FakePace::new());
        assert_eq!(ran.status, code(1), "{}", ran.err);
        let expected = format!(
            "js-agent cordon: credential error: cannot cordon the host: the server refused the \
             credential ({why})\n"
        );
        assert_eq!(ran.err, expected);
        assert!(ran.out.is_empty());
        assert!(!format!("{}{}", ran.out, ran.err).contains(token));
    }
}

/// Asserts what a restarted server says to a host that has not said hello since: the
/// credential is still the host's (it was saved with the binding) and the registry holds no
/// such host, so the client reports `UnknownHost` and the verb ends with status 1 saying the
/// server holds no such host. It catches an unknown host reported as a refused credential,
/// which would send an operator to the wrong place.
#[test]
fn a_restarted_server_that_has_not_heard_the_host_is_an_unknown_host() {
    let Enrolled {
        server,
        state,
        _files,
        _agent,
    } = enrol("cordon_http_unknown");
    let server = server.restart();
    let url = server.url();
    let credential = state.join(CREDENTIAL_FILE);
    let lease = HttpLease::connect(&url, &credential, None, "mac-1").unwrap();
    assert_eq!(lease.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.host().unwrap_err(), LeaseError::UnknownHost);

    let state_flag = state.display().to_string();
    let flags = [
        "--server",
        &url,
        "--state-root",
        &state_flag,
        "--host-id",
        "mac-1",
        "--for",
        "1h",
    ];
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
    let (url, served) = serve_garbage(4);
    let files = Scratch::new("cordon_http_garbage");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    let lease = HttpLease::connect(&url, &token, None, "mac-1").unwrap();
    for error in [
        lease.cordon(60).unwrap_err(),
        lease.uncordon().unwrap_err(),
        lease.host().unwrap_err(),
    ] {
        let LeaseError::Unreachable(text) = error else {
            panic!("{error:?}");
        };
        assert!(text.starts_with("the answer is not understood: "), "{text}");
    }
    let token = token.display().to_string();
    let flags = [
        "--server",
        &url,
        "--host-id",
        "mac-1",
        "--for",
        "1h",
        "--token-file",
        &token,
    ];
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(1));
    assert!(
        ran.err
            .contains("server error: cannot cordon the host: the answer is not understood")
    );
    served.join().unwrap();
}

/// Asserts that a server nothing listens on is a server error with status 1, naming the
/// address and not the credential. It catches an unreachable server reported as a usage error
/// (status 2) or as a refusal.
#[test]
fn a_server_that_cannot_be_reached_is_a_server_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let files = Scratch::new("cordon_http_closed");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    let token = token.display().to_string();
    let flags = [
        "--server",
        &url,
        "--host-id",
        "mac-1",
        "--for",
        "1h",
        "--token-file",
        &token,
    ];
    let ran = verb(&flags, &mut FakePace::new());
    assert_eq!(ran.status, code(1));
    let start =
        format!("js-agent cordon: server error: cannot cordon the host: cannot talk to {url}");
    assert!(ran.err.starts_with(&start), "{}", ran.err);
    assert!(!ran.err.contains("token-0123456789abcdef"));
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
    let (url, state) = (server.url(), enrolled.state_flag());
    let flags = [
        "--server",
        &url,
        "--state-root",
        &state,
        "--host-id",
        "mac-1",
        "--for",
        "100s",
        "--lease-secs",
        "30",
    ];
    let ran = verb(&flags, &mut pace);
    assert_eq!(ran.status, code(0), "{}", ran.err);
    assert_eq!(
        ran.out,
        "cordoned host=mac-1 expires_in_ms=30000 leases=0\nidle host=mac-1\nlifted host=mac-1\n"
    );
    assert_eq!(checks.get(), 10, "a renewal every 10 s of 100");
    assert!(server.host("mac-1").get("cordon").is_none());
}

/// Asserts that a cordon someone else placed is left alone: the verb exits with status 3
/// printing `kept`, and the server still shows the cordon afterwards. It catches an uncordon
/// sent for a cordon the verb did not place, against the real server's `already_cordoned`.
#[test]
fn another_holders_cordon_on_a_real_server_is_kept() {
    let enrolled = enrol("cordon_http_other");
    let other = enrolled.client();
    assert!(!other.cordon(600).unwrap().already_cordoned);
    let (url, state) = (enrolled.server.url(), enrolled.state_flag());
    let flags = [
        "--server",
        &url,
        "--state-root",
        &state,
        "--host-id",
        "mac-1",
        "--for",
        "1h",
    ];
    let mut pace = FakePace::new();
    let ran = verb(&flags, &mut pace);
    assert_eq!(ran.status, code(3), "{}", ran.err);
    assert_eq!(ran.out, "kept host=mac-1 reason=already_cordoned\n");
    assert!(pace.waits.is_empty());
    let cordon = enrolled.server.host("mac-1")["cordon"].clone();
    assert_eq!(cordon["expires_at_ms"], T0 + 600_000);
}
