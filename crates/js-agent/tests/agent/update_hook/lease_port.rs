//! The lease port of the hook on a machine ([`MachineLease`]) against a lease server in the
//! test process and against stub servers: a cordon placed, read and lifted; a host the server
//! does not know; a server that is down or never answers; and the credential file the params
//! name. The steps that loop on the server run over the fakes' clock, so their bound is the
//! step's and no test waits for it or measures it.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_agent::own_host::{CordonView, Lease, LeaseError};
use js_agent::update_hook::step::{self, Env};
use js_agent::update_hook::{
    Class, ConnectParams, Done, MachineLease, Outcome, POLL_SECS, Ports, Reason, Step,
    VERIFY_MARGIN_SECS,
};
use js_agent::{CREDENTIAL_FILE, REQUEST_LIMIT};

use rustix::net::{self, AddressFamily, SocketType};

use super::machine::Tree;
use super::step_rig::{Rig, Spy, as_root, running};
use crate::cordon::http::enrol;
use crate::server::T0;
use crate::support::Scratch;

/// The request limit of a test against a server that never answers: short, because every
/// request against it runs to the limit. Its length is not asserted on.
const SHORT: Duration = Duration::from_millis(200);

/// What the hook reports of a host the server does not know.
const UNKNOWN: &str = "unknown host (409 unknown-host): the lease server has not heard this host say hello since it started";

/// The port the params make, with the machine's limit unless `limit` says otherwise.
fn lease(params: &ConnectParams, limit: Duration) -> MachineLease {
    MachineLease::connect(params, &|| Err("unused".to_owned()), limit)
}

/// The params that reach `url` as `mac-1`, with the credential of `state` or of `token`.
fn reach(url: &str, state: Option<PathBuf>, token: Option<PathBuf>) -> ConnectParams {
    ConnectParams {
        server: Some(url.to_owned()),
        state_root: state,
        token_file: token,
        ca_cert: None,
        host_id: Some("mac-1".to_owned()),
    }
}

/// The text of a lease error the port reports as unreachable.
fn unreachable<T: std::fmt::Debug>(got: Result<T, LeaseError>) -> String {
    match got {
        Err(LeaseError::Unreachable(text)) => text,
        other => panic!("not an unreachable lease: {other:?}"),
    }
}

/// Runs `run` with the ports of `spy` but the lease `lease`.
fn with_lease(
    rig: &Rig,
    lease: &dyn Lease,
    run: fn(&Env<'_>, &js_agent::update_hook::Params) -> Done,
) -> Done {
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
fn cut_over(name: &str, bound: u64) -> Rig {
    let mut rig = running(Rig::new(name));
    let done = Spy::new(&rig).cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    rig.kill.disarm();
    rig.declaration.timeouts.verify = bound + VERIFY_MARGIN_SECS;
    rig
}

/// A server that takes every connection and never answers: each is held open until
/// [`Silent::stop`], which says how many there were.
struct Silent {
    url: String,
    addr: SocketAddr,
    done: Arc<AtomicBool>,
    thread: JoinHandle<usize>,
}

impl Silent {
    fn start() -> Silent {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&done);
        let thread = thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                if seen.load(Ordering::SeqCst) {
                    break;
                }
                held.push(stream.unwrap());
            }
            held.len()
        });
        Silent {
            url: format!("http://{addr}"),
            addr,
            done,
            thread,
        }
    }

    /// Stops taking connections (one last connection wakes the loop) and counts the others.
    fn stop(self) -> usize {
        self.done.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(self.addr).unwrap());
        self.thread.join().unwrap()
    }
}

/// An address nothing listens on, held for as long as the socket lives: bound and never
/// listening, so a connection to it is refused and no other test can be handed its port.
fn closed() -> (OwnedFd, String) {
    let socket = net::socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    net::bind(&socket, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = SocketAddrV4::try_from(net::getsockname(&socket).unwrap()).unwrap();
    (socket, format!("http://{addr}"))
}

/// Asserts the port against a live server with the credential the agent keeps in its state
/// root: the host reads uncordoned; a cordon of 120 s is placed on a host that was not
/// cordoned, with the server's expiry; a read between the cordon and the uncordon shows that
/// cordon, as does the server's own listing; the uncordon lifts it, and the read after it and
/// the listing show none; a second uncordon lifts nothing. It catches a port whose cordon or
/// uncordon does not reach the server, and one whose read does not show the cordon the server
/// holds (the reading `verify` and `prepare` decide on).
#[test]
fn the_machine_lease_cordons_reads_the_cordon_and_uncordons_on_a_live_server() {
    let enrolled = enrol("hook_lease_port_round_trip");
    let url = enrolled.server.url();
    let lease = lease(
        &reach(&url, Some(enrolled.state.clone()), None),
        REQUEST_LIMIT,
    );

    let before = lease.host().unwrap();
    assert_eq!(
        (before.id.as_str(), before.state.as_str(), before.cordon),
        ("mac-1", "live", None)
    );

    let placed = lease.cordon(120).unwrap();
    assert!(!placed.already_cordoned);
    let expiry = CordonView {
        expires_at_ms: T0 + 120_000,
        expires_in_ms: Some(120_000),
    };
    assert_eq!(placed.host.cordon, Some(expiry));
    let between = lease.host().unwrap();
    assert_eq!(between.cordon, Some(expiry), "the read shows the cordon");
    assert_eq!(
        enrolled.server.host("mac-1")["cordon"]["expires_at_ms"],
        T0 + 120_000
    );

    let lifted = lease.uncordon().unwrap();
    assert!(lifted.lifted);
    assert_eq!(lifted.host.cordon, None);
    assert_eq!(lease.host().unwrap().cordon, None);
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
    assert!(!lease.uncordon().unwrap().lifted);
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
        REQUEST_LIMIT,
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

/// Asserts that a server nothing listens on and one that takes the connection and never
/// answers both end each call of the port as an unreachable lease, the second at the request
/// limit the port was made with (a timeout); that `prepare` over either refuses with
/// `lease_unreachable`, class environment, and writes no record; and that `verify` over either
/// looks once per poll and ends at its own bound on the fakes' clock: 30 s of bound is three
/// pauses and four looks, each of which reached the silent server. It catches a port with no
/// request limit (the step would hang on a silent server past the updater's timeout), a
/// failure taken as an answer, and a step whose wait is not bounded by its clock.
#[test]
fn a_server_that_is_down_or_silent_is_a_bounded_unreachable_lease_and_the_steps_end_on_it() {
    let bound = 3 * POLL_SECS;
    let (_held, down) = closed();
    let silent = Silent::start();
    for (url, timeout) in [(down.as_str(), false), (silent.url.as_str(), true)] {
        let files = Scratch::new("hook-lease-port-down");
        let token = files.write("token", "token-0123456789abcdef", 0o600);
        let lease = lease(&reach(url, None, Some(token)), SHORT);
        for text in [
            unreachable(lease.host()),
            unreachable(lease.cordon(60)),
            unreachable(lease.uncordon()),
        ] {
            assert!(text.starts_with(&format!("cannot talk to {url}")), "{text}");
            assert_eq!(text.contains("timeout"), timeout, "{text}");
        }

        let rig = Rig::new("hook-lease-port-down-prepare");
        let done = with_lease(&rig, &lease, step::prepare);
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
        assert_eq!(done.report.class, Class::Environment);
        assert!(rig.record().is_none(), "nothing was written");

        let rig = cut_over("hook-lease-port-down-verify", bound);
        let ticks = rig.world.ticks();
        let done = with_lease(&rig, &lease, step::verify);
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
        assert_eq!(done.report.class, Class::Environment);
        let detail = done.report.detail.unwrap();
        assert!(
            detail.starts_with(&format!("cannot talk to {url}")),
            "{detail}"
        );
        assert_eq!(
            rig.world.ticks() - ticks,
            3,
            "one pause per look until the bound"
        );
        assert!(!rig.record().unwrap().verify_ok);
    }
    // Three calls of the port, one look of `prepare`, four of `verify`.
    assert_eq!(silent.stop(), 3 + 1 + 4);
}

/// Asserts that the port reads its bearer from the file `token_file` names when the state root
/// holds no credential, and reaches the server with it; that a token file that is missing, open
/// to others, of mode 0000 (unreadable, or refused by its mode when run as root) or a
/// directory is refused before any request, by the params key
/// `token_file` and the file's path, with no byte of the credential in the text; that a
/// credential in the state root that is open to others is refused by the key `state_root`; that
/// a bad server and CA file are named `server` and `ca_cert`; and that `prepare` over the
/// machine reports the missing token file in exactly those words. It catches a hook that names
/// a `--token-file` flag it does not have (or a token file it did not use), and one that would
/// print the credential in its report.
#[test]
fn the_credential_comes_from_the_file_the_params_name_and_a_bad_one_is_named_by_its_key() {
    let enrolled = enrol("hook_lease_port_token");
    let url = enrolled.server.url();
    let secret = fs::read_to_string(enrolled.state.join(CREDENTIAL_FILE)).unwrap();
    let files = Scratch::new("hook-lease-port-token");
    let token = files.write("token", &secret, 0o600);
    let reached = lease(&reach(&url, None, Some(token)), REQUEST_LIMIT);
    assert_eq!(reached.host().unwrap().id, "mac-1");

    let missing = files.path("no-such-token");
    let open = files.write("open", &secret, 0o644);
    let locked = files.write("locked", &secret, 0o000);
    let dir = files.path("a-directory");
    fs::create_dir_all(&dir).unwrap();
    let exact = format!(
        "token_file: {} cannot be read: No such file or directory (os error 2)",
        missing.display()
    );
    for (file, start) in [
        (&missing, exact.clone()),
        (
            &open,
            format!(
                "token_file: {} must be a regular file owned by you and readable only by you",
                open.display()
            ),
        ),
        (&locked, format!("token_file: {} ", locked.display())),
        (&dir, format!("token_file: {} ", dir.display())),
    ] {
        let lease = lease(&reach(&url, None, Some(file.clone())), REQUEST_LIMIT);
        for text in [
            unreachable(lease.host()),
            unreachable(lease.cordon(60)),
            unreachable(lease.uncordon()),
        ] {
            assert!(text.starts_with(&start), "{text}");
            assert!(!text.contains("--token-file"), "{text}");
            assert!(!text.contains(secret.trim()), "{text}");
        }
    }
    assert!(
        enrolled.server.host("mac-1").get("cordon").is_none(),
        "no refused file reached the server"
    );

    // The agent's own credential, made readable by others: the state root is named, as the
    // file the bearer was read from.
    let credential = enrolled.state.join(CREDENTIAL_FILE);
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o644)).unwrap();
    let text = unreachable(
        lease(
            &reach(&url, Some(enrolled.state.clone()), None),
            REQUEST_LIMIT,
        )
        .host(),
    );
    let start = format!(
        "state_root: {} must be a regular file owned by you",
        credential.display()
    );
    assert!(text.starts_with(&start), "{text}");
    fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();

    let token = files.write("token-2", &secret, 0o600);
    let off_loopback = reach("http://192.0.2.1:8443", None, Some(token.clone()));
    let text = unreachable(lease(&off_loopback, REQUEST_LIMIT).host());
    assert!(
        text.starts_with("server: \"http://192.0.2.1:8443\" is plain http"),
        "{text}"
    );
    let ca = files.path("no-such-ca.pem");
    let pinned = ConnectParams {
        ca_cert: Some(ca.clone()),
        ..reach("https://127.0.0.1:1", None, Some(token))
    };
    let text = unreachable(lease(&pinned, REQUEST_LIMIT).host());
    assert!(
        text.starts_with(&format!("ca_cert: {}", ca.display())),
        "{text}"
    );

    let tree = Tree::new("hook-lease-port-token-prepare");
    let lines = format!(
        "server = \"{url}\"\ntoken_file = \"{}\"\nhost_id = \"mac-1\"\n",
        missing.display()
    );
    let done = tree.run(Step::Prepare, &tree.params(true, &lines), Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.detail.as_deref(), Some(exact.as_str()));
}
