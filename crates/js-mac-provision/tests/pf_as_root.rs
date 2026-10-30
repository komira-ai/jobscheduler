//! The anchor for real, as root, on a Mac: `pfctl`, the packet filter and a process owned
//! by a pool uid. The unit tests run the rules' text and the install, check and remove
//! logic against a fake; this is the one test that shows pf then does what the text says.
//!
//! The test is `#[ignore]`: it needs root and loads rules into the Mac's packet filter
//! (under its own anchor, `com.apple/js-mac-provision-test`, which it flushes at the
//! end, and holds a pf enable reference for its duration). No job of this repository
//! runs it; a farm Mac runs it through a held script:
//!
//! ```text
//! cargo test -p js-mac-provision --test pf_as_root --no-run     # as an ordinary user
//! sudo -n <that test binary> --ignored --test-threads=1 --nocapture
//! ```
//!
//! The pool is the uids 4100 to 4109 and the control is uid 4200, outside it. None needs
//! to be an account: pf matches the uid of the socket. The probes are the program's own
//! `probe` verb, started as those uids from a world-readable copy of the program. The
//! endpoint the policy allows is a listener of the test on this Mac's own primary IPv4
//! address, so no outside host is needed; a second listener on the same address, on
//! another port, is the address "not on the list". The metadata address and a
//! documentation address (192.0.2.1 and 198.51.100.1 are reserved and never routed) are
//! the others.
//!
//! What it shows, with the control as the half that would pass without pf:
//! - a pool uid reaches the listed endpoint and is refused at once everywhere else (a
//!   `block return` rule answers with a reset, so "refused" is pf's answer: a connect
//!   that goes nowhere would time out instead);
//! - the same uid reaches IPv4 loopback;
//! - a uid outside the pool is refused by nothing;
//! - after `remove` the pool uid is no longer refused.
//!
//! The file builds on every platform, so clippy reads it on Linux; run anywhere but macOS,
//! the test stops at its first line.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_mac_provision::pfctl::SystemPfctl;
use js_mac_provision::provision::Provisioner;
use js_mac_provision::{AnchorName, Endpoint, UidRange};

const BIN: &str = env!("CARGO_BIN_EXE_js-mac-provision");
/// The tests' own directory, where every user may search.
const SCRATCH: &str = "/private/tmp/js-mac-provision-pf-as-root";
const ANCHOR: &str = "com.apple/js-mac-provision-test";
const POOL_FIRST: u32 = 4100;
const POOL_COUNT: u32 = 10;
const CONTROL_UID: u32 = 4200;
const METADATA: &str = "169.254.169.254:80";
const DOCUMENTATION: &str = "198.51.100.1:80";

/// Removes the anchor whatever happens to the test.
struct Loaded {
    anchor: AnchorName,
    state: PathBuf,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        let pf = SystemPfctl::new("/sbin/pfctl");
        if let Err(e) = Provisioner::new(&pf, &self.anchor, &self.state).remove() {
            eprintln!("could not remove the anchor {}: {e}", self.anchor);
        }
    }
}

/// This Mac's primary IPv4 address: the source address the kernel picks for a route to a
/// documentation address. A UDP connect sends nothing.
fn own_address() -> Ipv4Addr {
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind a UDP socket");
    socket
        .connect("192.0.2.1:9")
        .expect("this Mac has no route to send from (no default route?)");
    match socket.local_addr().expect("local address") {
        SocketAddr::V4(a) => *a.ip(),
        SocketAddr::V6(a) => panic!("expected an IPv4 address, got {a}"),
    }
}

/// What `probe` printed running as `uid`.
fn probe_as(program: &Path, uid: u32, addr: &str) -> String {
    let out = Command::new(program)
        .args(["probe", addr, "--timeout-ms", "2000"])
        .uid(uid)
        .gid(uid)
        .output()
        .unwrap_or_else(|e| panic!("start the probe as uid {uid} (is this root?): {e}"));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
#[ignore = "needs root on a Mac: loads pf rules under its own anchor; run through a held script"]
fn pool_uids_reach_only_the_listed_endpoint_and_other_uids_are_not_affected() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let scratch = PathBuf::from(SCRATCH);
    fs::create_dir_all(&scratch).expect("scratch dir");
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o755)).expect("scratch mode");
    let program = scratch.join("js-mac-provision");
    fs::copy(BIN, &program).expect("copy the program");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("program mode");

    let allowed = TcpListener::bind("0.0.0.0:0").expect("allowed listener");
    let unlisted = TcpListener::bind("0.0.0.0:0").expect("unlisted listener");
    let loopback = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let own = own_address();
    let port = |l: &TcpListener| l.local_addr().expect("listener address").port();
    let allowed_at = format!("{own}:{}", port(&allowed));
    let unlisted_at = format!("{own}:{}", port(&unlisted));
    let loopback_at = format!("127.0.0.1:{}", port(&loopback));

    let anchor = AnchorName::new(ANCHOR).expect("anchor name");
    let uids = UidRange::new(POOL_FIRST, POOL_COUNT).expect("uid range");
    let endpoints: Vec<Endpoint> = vec![allowed_at.parse().expect("endpoint")];
    let state = scratch.join("state");
    let pf = SystemPfctl::new("/sbin/pfctl");
    let provisioner = Provisioner::new(&pf, &anchor, &state);

    // Before the anchor: nothing refuses a pool uid, so a "refused" below is pf's.
    assert_eq!(
        probe_as(&program, POOL_FIRST, &unlisted_at),
        "connected",
        "before install, a pool uid reaches a listener on this Mac"
    );

    let _loaded = Loaded {
        anchor: anchor.clone(),
        state: state.clone(),
    };
    provisioner
        .install(uids, &endpoints)
        .expect("install the anchor");
    provisioner
        .check(uids, &endpoints)
        .expect("the loaded rules are the rendered ones");

    // The pool: the first and the last uid of the range.
    for uid in [POOL_FIRST, POOL_FIRST + POOL_COUNT - 1] {
        assert_eq!(
            probe_as(&program, uid, &allowed_at),
            "connected",
            "uid {uid}: listed"
        );
        assert_eq!(
            probe_as(&program, uid, &unlisted_at),
            "refused",
            "uid {uid}: same host, other port"
        );
        assert_eq!(
            probe_as(&program, uid, METADATA),
            "refused",
            "uid {uid}: metadata address"
        );
        assert_eq!(
            probe_as(&program, uid, DOCUMENTATION),
            "refused",
            "uid {uid}: unlisted address"
        );
        assert_eq!(
            probe_as(&program, uid, &loopback_at),
            "connected",
            "uid {uid}: loopback"
        );
    }
    // The uids either side of the range: the rules name `user 4100:4109` and no more.
    for uid in [POOL_FIRST - 1, POOL_FIRST + POOL_COUNT] {
        assert_eq!(
            probe_as(&program, uid, &unlisted_at),
            "connected",
            "uid {uid}: outside the range"
        );
    }

    // The control: a uid outside the pool is not affected. These would also hold with no
    // anchor at all, which is the point: pf changed nothing for them.
    assert_eq!(probe_as(&program, CONTROL_UID, &allowed_at), "connected");
    assert_eq!(probe_as(&program, CONTROL_UID, &unlisted_at), "connected");
    assert_ne!(
        probe_as(&program, CONTROL_UID, METADATA),
        "refused",
        "no rule matches the control uid (a time-out or a failure is the Mac having no such host)"
    );
    assert_ne!(probe_as(&program, CONTROL_UID, DOCUMENTATION), "refused");

    // Remove flushes the anchor: the pool uid is reaching the listener again.
    provisioner.remove().expect("remove the anchor");
    assert_eq!(
        probe_as(&program, POOL_FIRST, &unlisted_at),
        "connected",
        "after remove, a pool uid reaches a listener on this Mac again"
    );
}
