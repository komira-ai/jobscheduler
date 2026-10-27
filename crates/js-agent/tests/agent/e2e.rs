//! The agent against a real lease server in the test process: registration, leasing,
//! refusals, a server restart, the stop request, and the rate limit.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::{Connector, Dialer, Session, Step};
use js_agent_wire::ProtocolRange;
use serde_json::json;

use crate::server::{CLIENT_TOKEN, JOIN, OtherMajors, Redirect, Server, Spy};
use crate::support::{AMD_MAC, Host, PROFILER_CMD, Scratch, code, hello, lowest, mac_host};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A Mac without an Apple GPU: a host that offers no GPU at all.
fn mac_without_gpu() -> Host {
    let mut host = mac_host();
    host.commands = host.commands.with(PROFILER_CMD, Ok(AMD_MAC));
    host
}

/// Asserts the whole path of a host with no GPU: before the agent runs a lease request for
/// `os=macos` finds no capacity; once the agent has said hello (admitted because its
/// detected `os` is the pool's `macos`) the host is live, serving and listed with its
/// label; `POST /v1/leases` by capability leases it; a second request finds none; and the
/// host is leasable again after the lease is released. It proves a machine with no GPU
/// becomes leasable through the ordinary API with no new selector, and catches an agent
/// whose hello the server does not accept, or whose host the lease path cannot see.
#[test]
fn agent_registers_and_host_is_leasable() {
    let server = Server::start("e2e_leasable", &[]);
    let before = server.lease();
    assert_eq!(
        (before.status, before.body),
        (409, json!({ "error": "no-capacity" }))
    );

    let flags = ["--host-id", "mac-1", "--label", "rack=r1"];
    let agent = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &flags,
        mac_without_gpu,
        Dialer,
    );
    assert_eq!(agent.next_wait(), secs(5));

    let host = server.host("mac-1");
    assert_eq!(host["source"], "registered");
    assert_eq!(
        (host["state"].clone(), host["phase"].clone()),
        (json!("live"), json!("serving"))
    );
    assert_eq!(host["capabilities"], json!({ "os": "macos" }));
    assert_eq!(host["labels"], json!({ "rack": "r1" }));

    let granted = server.lease();
    assert_eq!(granted.status, 201, "{}", granted.body);
    assert_eq!(granted.body["host"], "mac-1");
    let id = granted.body["id"].as_str().unwrap().to_owned();
    assert_eq!(server.host("mac-1")["lease"], id.as_str());
    let full = server.lease();
    assert_eq!(full.body, json!({ "error": "no-capacity" }));

    agent.elapse();
    assert_eq!(agent.next_wait(), secs(5));
    let released = server.delete(&format!("/v1/leases/{id}"));
    assert_eq!(released.body, json!({ "state": "freed" }));
    assert_eq!(server.lease().status, 201);
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts that a Mac with an Apple GPU, whose hello says one GPU of class `apple-m`, is
/// admitted by the same pool and leased by the same request: the GPU facts the Mac probe
/// found are accepted by the server's wire checks.
#[test]
fn an_apple_gpu_host_is_admitted_and_leasable() {
    let server = Server::start("e2e_apple", &[]);
    let agent = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-gpu"],
        mac_host,
        Dialer,
    );
    agent.next_wait();
    assert_eq!(server.host("mac-gpu")["state"], "live");
    assert_eq!(server.lease().body["host"], "mac-gpu");
    agent.stop();
}

/// Asserts what each refusal of the real server does to the agent: a wrong token (401, no
/// body), a pool the server does not have (403 `unknown-pool`), a Linux host for a pool
/// that asks for `os=macos` (403 `shape-not-accepted`) and a second agent claiming a host
/// id that is live (409 `host-live`, which is waited out, not fatal). The first three end
/// the program with status 2 after one request and register nothing; none prints a token.
#[test]
fn refused_hello_is_fatal_per_code() {
    let server = Server::start("e2e_refused", &[]);
    let dir = Scratch::new("e2e_refused_files");
    let wrong = "wrong-token-0123456789-abcdefghijklmnop";
    let bad_token = dir.write("wrong", wrong, 0o600);

    let agent = crate::server::Agent::spawn(
        server.url(),
        bad_token,
        &["--host-id", "mac-1"],
        mac_host,
        Dialer,
    );
    let ran = agent.finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said =
        "js-agent: the server refused the hello with status 401 (unreadable); not retrying\n";
    assert_eq!(ran.err, said);
    assert!(!ran.err.contains(wrong) && !ran.err.contains(JOIN));
    assert!(
        server.get("/v1/hosts")["hosts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let nowhere = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1", "--pool", "nowhere"],
        mac_host,
        Dialer,
    );
    let ran = nowhere.finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("status 403 (unknown-pool)"), "{}", ran.err);

    let linux = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "linux-1"],
        crate::support::linux_host,
        Dialer,
    );
    let ran = linux.finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("status 403 (shape-not-accepted)"),
        "{}",
        ran.err
    );
    assert!(
        server.get("/v1/hosts")["hosts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let first = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1"],
        mac_host,
        Dialer,
    );
    first.next_wait();
    let second = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1"],
        mac_host,
        Dialer,
    );
    assert_eq!(second.next_wait(), Duration::from_millis(500));
    let ran = second.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err.contains("hello failed: status 409 (host-live)"),
        "{}",
        ran.err
    );
    first.stop();
}

/// Asserts that after the server restarts (a new process over the same files) the agent's
/// next heartbeat is refused as an unknown host, it says hello again at once and is
/// admitted, and the lease it held before the restart is still on the host, renewable,
/// with the host live again. It catches an agent that heartbeats a dead session for ever,
/// and a restart that loses the lease.
#[test]
fn server_restart_agent_reregisters_and_lease_resumes() {
    let server = Server::start("e2e_restart", &[]);
    let address = Arc::new(Mutex::new(server.url()));
    let agent = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1"],
        mac_host,
        Redirect(address.clone()),
    );
    agent.next_wait();
    agent.elapse();
    agent.next_wait();
    let granted = server.lease();
    assert_eq!(granted.status, 201, "{}", granted.body);
    let id = granted.body["id"].as_str().unwrap().to_owned();

    let server = server.restart();
    *address.lock().unwrap() = server.url();
    assert_eq!(server.host("mac-1")["state"], "awaiting");
    agent.elapse();
    assert_eq!(agent.next_wait(), Duration::ZERO);
    agent.elapse();
    assert_eq!(agent.next_wait(), secs(5));

    let host = server.host("mac-1");
    assert_eq!(host["state"], "live");
    assert_eq!(host["lease"], id.as_str());
    let renewed = server.post(&format!("/v1/leases/{id}/renew"));
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    agent.elapse();
    assert_eq!(agent.next_wait(), secs(5));
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err
            .contains("the server lost the session (unknown-host)"),
        "{}",
        ran.err
    );
}

/// Asserts that a stop request makes the agent send the phase `draining` and exit 0, that
/// the server then lists the host as draining, and that a lease request finds no capacity
/// on it: a draining host takes no new lease. It catches an agent that exits without
/// telling the server (the host then looks live until its grace runs out).
#[test]
fn sigterm_draining_then_no_new_lease() {
    let server = Server::start("e2e_drain", &[]);
    let agent = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1"],
        mac_host,
        Dialer,
    );
    agent.next_wait();
    assert_eq!(server.host("mac-1")["phase"], "serving");

    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err.contains("draining: the server was told"),
        "{}",
        ran.err
    );
    let host = server.host("mac-1");
    assert_eq!(
        (host["phase"].clone(), host["state"].clone()),
        (json!("draining"), json!("live"))
    );
    let none = server.lease();
    assert_eq!(
        (none.status, none.body),
        (409, json!({ "error": "no-capacity" }))
    );
}

/// Asserts that the hello rate limit is waited out as the server says: with a burst of one
/// the second agent's hello is answered 429 with a `Retry-After` of the refill interval,
/// the agent waits at least that (not the half second of its own backoff), and once the
/// server's clock has passed it the hello is admitted. It catches an agent that ignores the
/// header and retries into the limit, and a client that drops the header.
#[test]
fn a_rate_limited_hello_waits_for_retry_after() {
    let server = Server::start(
        "e2e_rate",
        &["--agent-hello-burst", "1", "--agent-hello-interval", "10s"],
    );
    let spawn = |host: &'static str| {
        crate::server::Agent::spawn(
            server.url(),
            server.join_file(),
            &["--host-id", host],
            mac_host,
            Dialer,
        )
    };
    let first = spawn("mac-1");
    assert_eq!(first.next_wait(), secs(5));
    let second = spawn("mac-2");
    assert_eq!(second.next_wait(), secs(10));
    server.clock.advance(secs(10));
    second.elapse();
    assert_eq!(second.next_wait(), secs(5));
    assert_eq!(server.host("mac-2")["state"], "live");
    let ran = second.stop();
    assert!(
        ran.err.contains("hello failed: status 429 (rate-limited)"),
        "{}",
        ran.err
    );
    first.stop();
}

/// Asserts, against the real server, that its refusal of a hello from an agent of other
/// majors is read as the server sends it: status 400, the code `unsupported-protocol` and
/// the majors the server speaks under `supported`. The scripted tests of the session use
/// bodies the agent's own authors wrote; this pins the shape to the server's type.
#[test]
fn the_servers_unsupported_protocol_refusal_reaches_the_session() {
    let server = Server::start("e2e_majors", &[]);
    let mut transport = OtherMajors(ProtocolRange { min: 2, max: 3 })
        .connect(&server.url(), &server.join_file(), None)
        .unwrap();
    let mut session = Session::new(hello("mac-1"), &mut *transport, lowest());
    let Step::Fatal(fatal) = session.tick() else {
        panic!("the server refuses the hello for good");
    };
    assert_eq!(
        (fatal.status, fatal.code.as_str()),
        (400, "unsupported-protocol")
    );
    assert_eq!(fatal.supported, Some(ProtocolRange::THIS));
}

/// Asserts that neither the join token nor the session secret the server issued appears on
/// stdout or stderr of a whole run, including a refused one, and that the token is not in
/// the program's arguments (only the path of its file is).
#[test]
fn token_absent_from_stderr() {
    let server = Server::start("e2e_secrets", &[]);
    let spy = Spy::default();
    let agent = crate::server::Agent::spawn(
        server.url(),
        server.join_file(),
        &["--host-id", "mac-1"],
        mac_host,
        spy.clone(),
    );
    agent.next_wait();
    agent.elapse();
    agent.next_wait();
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    let seen = spy.0.lock().unwrap();
    let secret = seen[0]["session"].as_str().unwrap();
    assert!(
        secret.len() >= 32,
        "the welcome carried no secret: {}",
        seen[0]
    );
    for text in [&ran.err, &ran.out] {
        assert!(!text.contains(secret), "{text}");
        assert!(!text.contains(JOIN), "{text}");
        assert!(!text.contains(CLIENT_TOKEN), "{text}");
    }
    assert!(ran.err.contains("registered, epoch 1"), "{}", ran.err);
}
