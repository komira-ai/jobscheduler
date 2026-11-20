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

use js_agent::own_host::{CordonView, HeldLease, HttpLease, Lease, LeaseError, RequestLimit};
use js_agent::{CREDENTIAL_FILE, Dialer, Login};
use js_client::ClientError;
use js_types::CordonHolder;
use serde_json::json;

use super::fakes::FakePace;
use super::verb::{Ran, drive_with};
use crate::jobs::private;
use crate::server::{Agent, JOIN, Server, T0};
use crate::support::{Host, Scratch, code, linux_host, mac_host};

fn holder(name: &str) -> CordonHolder {
    CordonHolder::new(name).expect("a holder")
}

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
    /// The agent that enrolled the host, held at its next wait.
    pub(crate) agent: Agent,
}

impl Enrolled {
    /// The state root as a flag value.
    pub(super) fn state_flag(&self) -> String {
        self.state.display().to_string()
    }

    /// The client of the host, with its own credential.
    fn client(&self) -> HttpLease {
        let credential = self.state.join(CREDENTIAL_FILE);
        HttpLease::connect(
            &self.server.url(),
            &Login::Token(credential),
            None,
            "mac-1",
            RequestLimit::DEFAULT,
        )
        .unwrap()
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
        agent,
    }
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

/// Runs the verb for real on `flags`, over a pace the test gave.
fn verb(flags: &[&str], pace: &mut FakePace<'_>) -> Ran {
    drive_with(flags, &linux_host(), &real, pace)
}

/// Asserts what the client does on the wire with the host's own credential: a read shows a
/// live host with no cordon and no mark; a cordon of 120 s says it was not cordoned before,
/// carries the server's expiry, which the server's host listing shows too; a second cordon
/// of a shorter lease says it was cordoned already and does not shorten it; an uncordon says
/// it lifted one and the listing shows none; a second uncordon says nothing was lifted. That
/// a cordoned host is placed no job is asserted with a container agent in `jobs/e2e.rs`. It
/// catches a client that sends the wrong
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

/// Asserts the refusals of a credential that is not the host's, as the client reports them:
/// the shared pool token is refused 403 `host-credential-required` on all three routes, and a
/// token the server does not know is a 401 with no body, which the client names `http-401`;
/// the verb ends with status 1, calls both a credential error and names the token file it
/// sent and the host it asked for, and neither the token nor
/// anything of it is on standard output or error. It catches a client that reports a refused
/// credential as an unreachable server, and a verb that prints the secret it was given.
#[test]
fn the_pool_token_and_an_unknown_token_are_refused_as_credential_errors() {
    let server = Server::start("cordon_http_refused", &[]);
    let files = Scratch::new("cordon_http_refused-files");
    let unknown = "an-unknown-token-0123456789abcdef";
    let unknown_file = files.write("unknown", unknown, 0o600);
    let url = server.url();

    let pooled = HttpLease::connect(
        &url,
        &Login::Token(server.join_file()),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
    let denied = || LeaseError::Refused("host-credential-required".to_owned());
    assert_eq!(pooled.cordon(60).unwrap_err(), denied());
    assert_eq!(pooled.uncordon().unwrap_err(), denied());
    assert_eq!(pooled.host().unwrap_err(), denied());
    let stranger = HttpLease::connect(
        &url,
        &Login::Token(unknown_file.clone()),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
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
             credential ({why})\njs-agent cordon: it sent the token file {file} for host mac-1; \
             to cordon with what the agent on this host presents, give the agent's flags file \
             with --flags-file (such as /etc/js-agent/flags)\n"
        );
        assert_eq!(ran.err, expected);
        assert!(ran.out.is_empty());
        assert!(!format!("{}{}", ran.out, ran.err).contains(token));
    }
}

/// Asserts what a restarted server says to a host that said hello before the restart: the
/// server holds the host's session in its row (the job tables persist), so with no hello since
/// the host's own credential places a cordon on it (live, epoch 1, the session it had), the
/// read then shows the cordon, the lift is served and the verb ends with status 0. It catches a restarted
/// server that answers a host it can read from its rows as unknown, which would make the
/// operator's cordon fail until the agent's next hello.
#[test]
fn a_restarted_server_serves_the_host_its_rows_hold() {
    let Enrolled {
        server,
        state,
        _files,
        agent: _agent,
    } = enrol("cordon_http_restarted");
    let server = server.restart();
    let url = server.url();
    let credential = state.join(CREDENTIAL_FILE);
    let lease = HttpLease::connect(
        &url,
        &Login::Token(credential),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
    let placed = lease.cordon(60).unwrap();
    assert!(!placed.already_cordoned);
    assert_eq!((placed.host.state.as_str(), placed.host.epoch), ("live", 1));
    assert!(lease.host().unwrap().cordon.is_some());
    lease.uncordon().unwrap();

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
    assert_eq!(ran.status, code(0), "{}", ran.err);
}

/// Asserts what a server whose job tables hold no row for the host says to it: the credential
/// is still the host's (it was saved with the binding) and the tables hold no such host, so
/// the client reports `UnknownHost` and the verb ends with status 1 saying the server holds
/// no such host. It catches an unknown host reported as a refused credential, which would send
/// an operator to the wrong place.
#[cfg(target_os = "linux")]
#[test]
fn a_server_whose_tables_hold_no_row_for_the_host_says_unknown_host() {
    let Enrolled {
        server,
        state,
        _files,
        agent: _agent,
    } = enrol("cordon_http_unknown");
    let server = server.restart_forgetting_jobs();
    let url = server.url();
    let credential = state.join(CREDENTIAL_FILE);
    let lease = HttpLease::connect(
        &url,
        &Login::Token(credential),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
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
    let (url, served) = serve_garbage(6);
    let files = Scratch::new("cordon_http_garbage");
    let token = files.write("token", "token-0123456789abcdef", 0o600);
    let lease = HttpLease::connect(
        &url,
        &Login::Token(token.clone()),
        None,
        "mac-1",
        RequestLimit::DEFAULT,
    )
    .unwrap();
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
        "--holder",
        "ops-1",
    ];
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
            "--holder",
            "ops-1",
        ];
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
    let login = Login::Token(token);
    let lease = HttpLease::connect(&url, &login, None, "mac-1", RequestLimit::DEFAULT).unwrap();
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
