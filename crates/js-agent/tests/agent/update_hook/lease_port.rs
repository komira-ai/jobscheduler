//! The lease port of the hook on a machine ([`MachineLease`]) against a lease server in the
//! test process: the reading `prepare` and `verify` decide on (the cordon and the session), a
//! cordon placed, read and lifted at every boundary of its duration, a host the server does not
//! know, and the refusals of a server or a CA the params name wrongly. The transport's failures
//! are in `lease_port_transport.rs`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::Dialer;
use js_agent::own_host::{
    CordonView, HostReading, HttpLease, Lease, LeaseError, MAX_CORDON_SECS, RequestLimit,
};
use js_agent::update_hook::step::Env;
use js_agent::update_hook::{
    Class, ConnectParams, Done, MachineLease, Outcome, Params, Ports, Reason, Step,
    VERIFY_MARGIN_SECS,
};
use js_client::{Client, HostIdentity};

use super::lease_stub::{Reply, Stub};
use super::machine::{Tree, reaching_as_mac_1};
use super::step_rig::{Rig, Spy, as_root, running};
use crate::cordon::http::enrol;
use crate::host_certs::self_signed;
use crate::server::{Agent, Server, T0};
use crate::support::{Scratch, code, mac_host};

/// What the hook reports of a host the server does not know.
const UNKNOWN: &str = "unknown host (409 unknown-host): the lease server has not heard this host say hello since it started";

/// The port the params make.
pub(super) fn lease(params: &ConnectParams, limit: RequestLimit) -> MachineLease {
    MachineLease::connect(params, &|| Err("unused".to_owned()), limit)
}

/// The own-host routes of `mac-1` at the plain-HTTP stub `url`, with the bearer in `token`: the
/// client the port is made of, over a server of the test's own that answers what the test says.
pub(super) fn plain_lease(url: &str, token: &Path, limit: RequestLimit) -> HttpLease {
    let client = Client::connect(url, token, None, limit.get()).unwrap();
    HttpLease::over(client, "mac-1")
}

/// A limit short enough for a test whose every call runs to it; its length is not asserted on.
pub(super) fn short() -> RequestLimit {
    RequestLimit::new(Duration::from_millis(200)).unwrap()
}

/// The params that reach `url` as `mac-1`, with the host certificate and key `identity` and
/// the certificate `ca` to trust.
pub(super) fn reach(
    url: &str,
    identity: Option<HostIdentity>,
    ca: Option<PathBuf>,
) -> ConnectParams {
    ConnectParams {
        server: Some(url.to_owned()),
        ca_cert: ca,
        host_id: Some("mac-1".to_owned()),
        host_identity: identity,
    }
}

/// The params that reach `server` as its host `mac-1`, by the host's certificate and key.
pub(super) fn reach_server(server: &Server) -> ConnectParams {
    let identities = server.identities();
    reach(
        &server.url(),
        Some(identities.of("mac-1")),
        Some(identities.server_cert().to_owned()),
    )
}

/// The text of a lease error the port reports as unreachable.
pub(super) fn unreachable<T: std::fmt::Debug>(got: Result<T, LeaseError>) -> String {
    match got {
        Err(LeaseError::Unreachable(text)) => text,
        other => panic!("not an unreachable lease: {other:?}"),
    }
}

/// The three calls of `lease`, each as the text of its unreachable error.
pub(super) fn all_unreachable(lease: &dyn Lease) -> [String; 3] {
    [
        unreachable(lease.host()),
        unreachable(lease.cordon(60)),
        unreachable(lease.uncordon()),
    ]
}

/// Runs `run` with the ports of the fakes but the lease `lease`.
pub(super) fn with_lease(rig: &Rig, lease: &dyn Lease, run: fn(&Env<'_>, &Params) -> Done) -> Done {
    let spy = Spy::new(rig);
    let env = spy.env(rig, &as_root);
    let env = Env {
        ports: Ports { lease, ..env.ports },
        ..env
    };
    run(&env, &rig.params)
}

/// A host `cutover` has finished on (over the fakes), whose `verify` waits at most `bound`
/// seconds on the fakes' clock.
pub(super) fn cut_over(name: &str, bound: u64) -> Rig {
    let mut rig = running(Rig::new(name));
    let done = Spy::new(&rig).cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    rig.kill.disarm();
    rig.params.settings.timeouts.verify = bound + VERIFY_MARGIN_SECS;
    rig
}

/// Asserts the reading of a live host through the port, field by field, as the steps decide on
/// it: live, the first session (epoch 1, begun at the server's start time), the agent's version,
/// no runtime reported, no lease, mark, taint or cordon. A cordon of 120 s is placed on a host
/// that was not cordoned, with the server's expiry; a read between the cordon and the uncordon
/// is the same reading with that cordon, as the server's own listing shows; the uncordon lifts
/// it, and the read after it is the first reading again; a second uncordon lifts nothing. At the
/// boundaries of the duration: 0 s is refused `invalid-cordon` and changes nothing; the longest
/// cordon, one second more, and `u64::MAX` are each placed for exactly the longest cordon. It
/// catches a port whose cordon or uncordon does not reach the server, whose read drops or
/// misreads a field `verify` compares, or that takes a refused cordon as placed.
#[test]
fn the_machine_lease_cordons_reads_and_uncordons_a_live_host_at_every_duration_boundary() {
    let enrolled = enrol("hook_lease_port_round_trip");
    let lease = lease(&reach_server(&enrolled.server), RequestLimit::DEFAULT);

    let first = HostReading {
        id: "mac-1".to_owned(),
        state: "live".to_owned(),
        epoch: 1,
        session_started_ms: Some(T0),
        leases: 0,
        lease: None,
        releasing: false,
        mark: None,
        cordon: None,
        taint: None,
        agent_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        runtime: None,
    };
    assert_eq!(lease.host().unwrap(), first);

    let placed = lease.cordon(120).unwrap();
    assert!(!placed.already_cordoned);
    let expiry = CordonView {
        expires_at_ms: T0 + 120_000,
        expires_in_ms: Some(120_000),
        holder: None,
    };
    let cordoned = HostReading {
        cordon: Some(expiry.clone()),
        ..first.clone()
    };
    assert_eq!(placed.host, cordoned);
    assert_eq!(lease.host().unwrap(), cordoned, "the read shows the cordon");
    assert_eq!(
        enrolled.server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 120_000
    );

    let lifted = lease.uncordon().unwrap();
    assert!(lifted.lifted);
    assert_eq!(lifted.host, first);
    assert_eq!(lease.host().unwrap(), first);
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
    assert!(!lease.uncordon().unwrap().lifted);

    assert_eq!(
        lease.cordon(0).unwrap_err(),
        LeaseError::Refused("invalid-cordon".to_owned())
    );
    assert_eq!(
        lease.host().unwrap(),
        first,
        "a refused cordon changes nothing"
    );
    let longest = CordonView {
        expires_at_ms: T0 + MAX_CORDON_SECS * 1000,
        expires_in_ms: Some(MAX_CORDON_SECS * 1000),
        holder: None,
    };
    for asked in [MAX_CORDON_SECS, MAX_CORDON_SECS + 1, u64::MAX] {
        let placed = lease.cordon(asked).unwrap();
        assert!(!placed.already_cordoned, "{asked}");
        assert_eq!(placed.host.cordon.as_ref(), Some(&longest), "{asked}");
        assert!(lease.uncordon().unwrap().lifted, "{asked}");
    }
}

/// Asserts what the port reads of a session ending and another beginning, which is what
/// `verify` and `rollback` tell an agent's new start by: after the agent stops, the server still
/// shows its session; once the grace passes on the server's clock the host reads `lost` with
/// that session and nothing else changed; a new agent's hello with the same certificate makes it
/// `live` again with epoch 2 and a session begun at the server's time of the hello, which
/// differs from the first. It catches a port that reads a stale or cached session, or drops the
/// epoch or the stamp.
#[test]
fn the_machine_lease_reads_a_session_end_and_the_next_session_of_a_live_host() {
    let enrolled = enrol("hook_lease_port_session");
    let url = enrolled.server.url();
    let lease = lease(&reach_server(&enrolled.server), RequestLimit::DEFAULT);
    let first = lease.host().unwrap();
    assert_eq!(
        (first.state.as_str(), first.epoch, first.session_started_ms),
        ("live", 1, Some(T0))
    );

    let stopped = enrolled.agent.stop();
    assert_eq!(stopped.status(), code(0), "{}", stopped.err);
    assert_eq!(lease.host().unwrap(), first, "the server holds the session");

    enrolled.server.clock.advance(Duration::from_secs(70));
    let lost = lease.host().unwrap();
    assert_eq!(lost.state, "lost");
    assert_eq!(
        HostReading {
            state: "live".to_owned(),
            ..lost
        },
        first,
        "only the state changes"
    );

    let state = enrolled.state.display().to_string();
    let flags = ["--host-id", "mac-1", "--state-root", &state];
    let again = Agent::spawn(url, enrolled.server.identities(), &flags, mac_host, Dialer);
    assert_eq!(again.next_wait(), Duration::from_secs(5), "the hello");
    let next = lease.host().unwrap();
    assert_eq!(
        (next.state.as_str(), next.epoch, next.session_started_ms),
        ("live", 2, Some(T0 + 70_000))
    );
    assert_ne!(next.session_started_ms, first.session_started_ms);
    let ran = again.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts what a restarted server says through the port to a host that said hello before the
/// restart: its row holds the session, so the cordon is placed on the live host, the read shows
/// it and the uncordon lifts it. It catches a restarted server that answers a host its rows hold as unknown,
/// which would make `prepare` refuse with `lease_unreachable` until the agent's next hello.
#[test]
fn a_restarted_server_serves_the_host_its_rows_hold_through_the_port() {
    let enrolled = enrol("hook_lease_port_restarted");
    let server = enrolled.server.restart();
    let lease = lease(&reach_server(&server), RequestLimit::DEFAULT);
    let placed = lease.cordon(60).unwrap();
    assert_eq!((placed.host.state.as_str(), placed.host.epoch), ("live", 1));
    assert!(lease.host().unwrap().cordon.is_some());
    assert!(lease.uncordon().is_ok());
    assert!(lease.host().unwrap().cordon.is_none());
    assert_eq!(server.host("mac-1")["state"], "live");
}

/// Asserts what a server whose job tables hold no row for the host says through the port: the
/// read, the cordon and the uncordon are each the typed `UnknownHost` (the server's 409
/// `unknown-host`), and `prepare` over the machine refuses with exit 1, `lease_unreachable`,
/// class environment and a detail naming the 409, writes no record and leaves the server with
/// no such host. It catches an unknown host taken as a host with no cordon (a step that goes
/// on as if it had drained), reported as a refused credential, or blamed on the release.
#[cfg(target_os = "linux")]
#[test]
fn a_host_the_server_does_not_know_is_the_typed_unknown_host_and_prepare_refuses() {
    let enrolled = enrol("hook_lease_port_unknown");
    let server = enrolled.server.restart_forgetting_jobs();
    let lease = lease(&reach_server(&server), RequestLimit::DEFAULT);
    assert_eq!(lease.host().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.uncordon().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(LeaseError::UnknownHost.to_string(), UNKNOWN);

    let tree = Tree::new("hook-lease-port-unknown");
    let lines = reaching_as_mac_1(&server, &enrolled.state);
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.detail.as_deref(), Some(UNKNOWN));
    assert!(!tree.record().exists(), "nothing was written");
    assert_eq!(server.get("/v1/hosts")["hosts"], serde_json::json!([]));
}

/// Asserts that a server the params name wrongly and a CA file that is not there are named in
/// exact words (`--server` and `--ca-cert`, as the client says them), that no refused client
/// sends anything (a counting server sees no connection), and that a host certificate and key
/// the client cannot use is named by its flag and file. It catches a text that names a flag the
/// hook does not have, one that carries any part of the key, and a refused client that still
/// sends a read or an uncordon.
#[test]
fn a_bad_server_a_missing_ca_and_an_unusable_key_are_named_exactly_and_send_nothing() {
    let files = Scratch::new("hook-lease-port-refused");
    let identity = self_signed(&files, "mac-1");
    let counting = Stub::start(&[Reply::Answer(200, "{}")]);

    let off_loopback = reach("http://192.0.2.1:8443", Some(identity.clone()), None);
    assert_eq!(
        unreachable(lease(&off_loopback, RequestLimit::DEFAULT).host()),
        "--server: \"http://192.0.2.1:8443\" is plain http to a host off the loopback: a bearer \
         token never crosses a network in clear, use https://"
    );
    let ca = files.path("no-such-ca.pem");
    let pinned = reach(
        &counting.https_url(),
        Some(identity.clone()),
        Some(ca.clone()),
    );
    assert_eq!(
        unreachable(lease(&pinned, RequestLimit::DEFAULT).host()),
        format!(
            "--ca-cert: {}: I/O error: No such file or directory (os error 2)",
            ca.display()
        )
    );
    let gone = HostIdentity::new(files.path("no-such.crt"), identity.key_path());
    for text in all_unreachable(&lease(
        &reach(&counting.https_url(), Some(gone.clone()), None),
        RequestLimit::DEFAULT,
    )) {
        assert!(
            text.starts_with(&format!("--host-cert: {}", gone.cert_path().display())),
            "{text}"
        );
    }
    assert_eq!(counting.stop(), 0, "no refused client sent a request");

    let tree = Tree::new("hook-lease-port-refused-prepare");
    // A private state root, as an update needs: the lease is then asked for, and refused.
    let lines = format!(
        "server = \"http://192.0.2.1:8443\"\nhost_id = \"mac-1\"\n{}",
        tree.state_line()
    );
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert!(
        done.report
            .detail
            .as_deref()
            .unwrap()
            .starts_with("--server: "),
        "{done:?}"
    );
}
