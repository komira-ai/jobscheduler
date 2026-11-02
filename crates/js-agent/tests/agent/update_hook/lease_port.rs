//! The lease port of the hook on a machine ([`MachineLease`]) against a lease server in the
//! test process: the reading `prepare` and `verify` decide on (the cordon and the session), a
//! cordon placed, read and lifted at every boundary of its duration, a host the server does not
//! know, and the credential the params lead to. The transport's failures are in
//! `lease_port_transport.rs`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::own_host::{
    CordonView, HostReading, Lease, LeaseError, MAX_CORDON_SECS, RequestLimit,
};
use js_agent::update_hook::step::Env;
use js_agent::update_hook::{
    Class, ConnectParams, Done, MachineLease, Outcome, Params, Ports, Reason, Step,
    VERIFY_MARGIN_SECS,
};
use js_agent::{CREDENTIAL_FILE, Dialer};

use super::lease_stub::{Reply, Stub};
use super::machine::Tree;
use super::step_rig::{Rig, Spy, as_root, running};
use crate::cordon::http::enrol;
use crate::server::{Agent, T0};
use crate::support::{Host, Scratch, code, mac_host};

/// What the hook reports of a host the server does not know.
const UNKNOWN: &str = "unknown host (409 unknown-host): the lease server has not heard this host say hello since it started";

/// The refusal of a credential file that others may read, as `js-client` words it.
const NOT_PRIVATE: &str =
    "must be a regular file owned by you and readable only by you (mode 0600 or 0400)";

/// The port the params make.
pub(super) fn lease(params: &ConnectParams, limit: RequestLimit) -> MachineLease {
    MachineLease::connect(params, &|| Err("unused".to_owned()), limit)
}

/// A limit short enough for a test whose every call runs to it; its length is not asserted on.
pub(super) fn short() -> RequestLimit {
    RequestLimit::new(Duration::from_millis(200)).unwrap()
}

/// The params that reach `url` as `mac-1`, with the credential of `state` or of `token`.
pub(super) fn reach(url: &str, state: Option<PathBuf>, token: Option<PathBuf>) -> ConnectParams {
    ConnectParams {
        server: Some(url.to_owned()),
        state_root: state,
        token_file: token,
        ca_cert: None,
        host_id: Some("mac-1".to_owned()),
    }
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
    rig.declaration.timeouts.verify = bound + VERIFY_MARGIN_SECS;
    rig
}

/// A Mac whose instance file says it runs on `lambdalabs:i-1`, as the enrolled host's.
fn launched() -> Host {
    let mut host = mac_host();
    host.files = host.files.with("/instance", "lambdalabs:i-1\n");
    host
}

/// The uid the test runs as.
fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// What `js-client` says of a credential file at `path` with `mode` that it opened and would
/// not trust.
fn not_private(key: &str, path: &Path, mode: u32) -> String {
    format!(
        "{key}: {} {NOT_PRIVATE}, not mode {mode:04o} owned by uid {}",
        path.display(),
        uid()
    )
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
    let url = enrolled.server.url();
    let lease = lease(
        &reach(&url, Some(enrolled.state.clone()), None),
        RequestLimit::DEFAULT,
    );

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
    };
    let cordoned = HostReading {
        cordon: Some(expiry),
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
    };
    for asked in [MAX_CORDON_SECS, MAX_CORDON_SECS + 1, u64::MAX] {
        let placed = lease.cordon(asked).unwrap();
        assert!(!placed.already_cordoned, "{asked}");
        assert_eq!(placed.host.cordon, Some(longest), "{asked}");
        assert!(lease.uncordon().unwrap().lifted, "{asked}");
    }
}

/// Asserts what the port reads of a session ending and another beginning, which is what
/// `verify` and `rollback` tell an agent's new start by: after the agent stops, the server still
/// shows its session; once the grace passes on the server's clock the host reads `lost` with
/// that session and nothing else changed; a new agent's hello on the same credential makes it
/// `live` again with epoch 2 and a session begun at the server's time of the hello, which
/// differs from the first. It catches a port that reads a stale or cached session, or drops the
/// epoch or the stamp.
#[test]
fn the_machine_lease_reads_a_session_end_and_the_next_session_of_a_live_host() {
    let enrolled = enrol("hook_lease_port_session");
    let url = enrolled.server.url();
    let lease = lease(
        &reach(&url, Some(enrolled.state.clone()), None),
        RequestLimit::DEFAULT,
    );
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
    let flags = [
        "--host-id",
        "mac-1",
        "--instance-file",
        "/instance",
        "--state-root",
        &state,
    ];
    let again = Agent::spawn_tokenless(url, &flags, launched, Dialer);
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

/// Asserts what a restarted server that has not heard the host since says through the port:
/// the read, the cordon and the uncordon are each the typed `UnknownHost` (the server's 409
/// `unknown-host`), and `prepare` over the machine refuses with exit 1, `lease_unreachable`,
/// class environment and a detail naming the 409, writes no record and leaves the server with
/// no such host. It catches an unknown host taken as a host with no cordon (a step that goes
/// on as if it had drained), reported as a refused credential, or blamed on the release.
#[test]
fn a_host_the_server_does_not_know_is_the_typed_unknown_host_and_prepare_refuses() {
    let enrolled = enrol("hook_lease_port_unknown");
    let server = enrolled.server.restart();
    let url = server.url();
    let lease = lease(
        &reach(&url, Some(enrolled.state.clone()), None),
        RequestLimit::DEFAULT,
    );
    assert_eq!(lease.host().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.cordon(60).unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(lease.uncordon().unwrap_err(), LeaseError::UnknownHost);
    assert_eq!(LeaseError::UnknownHost.to_string(), UNKNOWN);

    let tree = Tree::new("hook-lease-port-unknown");
    let lines = format!(
        "server = \"{url}\"\nstate_root = \"{}\"\nhost_id = \"mac-1\"\n",
        enrolled.state.display()
    );
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.detail.as_deref(), Some(UNKNOWN));
    assert!(!tree.record().exists(), "nothing was written");
    assert_eq!(server.get("/v1/hosts")["hosts"], serde_json::json!([]));
}

/// Asserts that the port reads its bearer from the file `token_file` names when the state root
/// holds no credential, and reaches the server with it; that a token file that is missing, open
/// to others, of mode 0000 (unreadable, or refused by its mode when the test runs as root) or a
/// directory is refused before any request, in exactly the words that name the params key
/// `token_file` and the file; that a bad server and a missing CA file are named `server` and
/// `ca_cert` in exact words; that no refused client sends anything (a counting server sees no
/// connection); and that `prepare` over the machine reports the missing token file in exactly
/// those words. It catches a hook that names a `--token-file` flag it does not have, a text
/// that carries any part of the credential, and a refused credential that still sends a read or
/// an uncordon.
#[test]
fn the_credential_comes_from_the_token_file_the_params_name_and_a_bad_one_is_named_exactly() {
    let enrolled = enrol("hook_lease_port_token");
    let url = enrolled.server.url();
    let secret = fs::read_to_string(enrolled.state.join(CREDENTIAL_FILE)).unwrap();
    let files = Scratch::new("hook-lease-port-token");
    let token = files.write("token", &secret, 0o600);
    let reached = lease(
        &reach(&url, None, Some(token.clone())),
        RequestLimit::DEFAULT,
    );
    assert_eq!(reached.host().unwrap().id, "mac-1");

    let missing = files.path("no-such-token");
    let open = files.write("open", &secret, 0o644);
    let locked = files.write("locked", &secret, 0o000);
    let dir = files.path("a-directory");
    fs::create_dir_all(&dir).unwrap();
    let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    let gone = format!(
        "token_file: {} cannot be read: No such file or directory (os error 2)",
        missing.display()
    );
    let unreadable = match uid() {
        0 => not_private("token_file", &locked, 0),
        _ => format!(
            "token_file: {} cannot be read: Permission denied (os error 13)",
            locked.display()
        ),
    };
    let counting = Stub::start(&[Reply::Answer(200, "{}")]);
    for (file, want) in [
        (&missing, gone.clone()),
        (&open, not_private("token_file", &open, 0o644)),
        (&locked, unreadable),
        (&dir, not_private("token_file", &dir, dir_mode)),
    ] {
        let lease = lease(
            &reach(&counting.url, None, Some(file.clone())),
            RequestLimit::DEFAULT,
        );
        for text in all_unreachable(&lease) {
            assert_eq!(text, want);
        }
    }

    let off_loopback = reach("http://192.0.2.1:8443", None, Some(token.clone()));
    assert_eq!(
        unreachable(lease(&off_loopback, RequestLimit::DEFAULT).host()),
        "server: \"http://192.0.2.1:8443\" is plain http to a host off the loopback: a bearer \
         token never crosses a network in clear, use https://"
    );
    let ca = files.path("no-such-ca.pem");
    let pinned = ConnectParams {
        ca_cert: Some(ca.clone()),
        ..reach(&counting.https_url(), None, Some(token))
    };
    assert_eq!(
        unreachable(lease(&pinned, RequestLimit::DEFAULT).host()),
        format!(
            "ca_cert: {}: I/O error: No such file or directory (os error 2)",
            ca.display()
        )
    );
    assert_eq!(counting.stop(), 0, "no refused client sent a request");

    let tree = Tree::new("hook-lease-port-token-prepare");
    let lines = format!(
        "server = \"{url}\"\ntoken_file = \"{}\"\nhost_id = \"mac-1\"\n",
        missing.display()
    );
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.detail.as_deref(), Some(gone.as_str()));
}

/// Asserts the precedence of the two credentials and the refusal of a damaged saved one: with
/// a valid credential in the state root and a token file that does not exist, the saved
/// credential is used and reaches the server; a saved credential that is empty, malformed, a
/// directory, open to others or of mode 0000 is refused in exact words naming the key
/// `state_root` and the file, even with a usable token file beside it, and nothing is sent (a
/// counting server sees no connection); and `prepare` reports a malformed one in exactly those
/// words. It catches a hook that falls back to the token file past a damaged credential (the
/// agent would not), one that names the token file for the state root's credential, and one
/// whose text carries the credential.
#[test]
fn the_state_roots_credential_comes_first_and_a_damaged_one_is_refused_by_its_key() {
    let enrolled = enrol("hook_lease_port_saved");
    let url = enrolled.server.url();
    let state = enrolled.state.clone();
    let saved = state.join(CREDENTIAL_FILE);
    let secret = fs::read_to_string(&saved).unwrap();
    let files = Scratch::new("hook-lease-port-saved");
    let missing = files.path("no-such-token");
    let both = lease(
        &reach(&url, Some(state.clone()), Some(missing)),
        RequestLimit::DEFAULT,
    );
    assert_eq!(
        both.host().unwrap().id,
        "mac-1",
        "the saved credential is used"
    );

    let token = files.write("token", &secret, 0o600);
    let counting = Stub::start(&[Reply::Answer(200, "{}")]);
    let refused = |want: &str| {
        let lease = lease(
            &reach(&counting.url, Some(state.clone()), Some(token.clone())),
            RequestLimit::DEFAULT,
        );
        for text in all_unreachable(&lease) {
            assert_eq!(text, want);
        }
    };
    let holds_none = format!(
        "state_root: {}: does not hold a host credential",
        saved.display()
    );
    for content in ["", "not a credential!!"] {
        fs::write(&saved, content).unwrap();
        refused(&holds_none);
    }
    fs::remove_file(&saved).unwrap();
    fs::create_dir(&saved).unwrap();
    refused(&format!(
        "state_root: {}: cannot be read: is a directory",
        saved.display()
    ));
    fs::remove_dir(&saved).unwrap();
    fs::write(&saved, &secret).unwrap();
    fs::set_permissions(&saved, fs::Permissions::from_mode(0o644)).unwrap();
    refused(&not_private("state_root", &saved, 0o644));
    fs::set_permissions(&saved, fs::Permissions::from_mode(0o000)).unwrap();
    refused(&match uid() {
        0 => not_private("state_root", &saved, 0),
        _ => format!(
            "state_root: {}: cannot be read: permission denied",
            saved.display()
        ),
    });
    assert_eq!(counting.stop(), 0, "no refused client sent a request");

    fs::set_permissions(&saved, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&saved, "not a credential!!").unwrap();
    let tree = Tree::new("hook-lease-port-saved-prepare");
    let lines = format!(
        "server = \"{url}\"\nstate_root = \"{}\"\ntoken_file = \"{}\"\nhost_id = \"mac-1\"\n",
        state.display(),
        token.display()
    );
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.detail.as_deref(), Some(holds_none.as_str()));
    assert!(!tree.record().exists());
}
