//! The anchor for real, as root, on a Mac: `pfctl`, the packet filter and a process owned
//! by a pool uid. The unit tests run the rules' text and the install, check and remove
//! logic against a fake; this is the one test that shows pf then does what the text says.
//!
//! The test is `#[ignore]`: it needs root and loads rules into the Mac's packet filter
//! (under its own anchor, `com.apple/js-mac-provision-test`, which it flushes at the
//! end, and holds a pf enable reference for its duration). No pull request job runs it. The
//! manual workflow `js-mac-provision-privileged-tests` builds it for Apple silicon, with
//! the program it starts, and uploads both without running them; a farm Mac runs it through
//! a held script:
//!
//! ```text
//! cargo test -p js-mac-provision --test pf_as_root --no-run     # as an ordinary user
//! sudo -n env JS_MAC_PROVISION_BIN=<the js-mac-provision built beside it> \
//!     <that test binary> --ignored --test-threads=1 --nocapture
//! ```
//!
//! `JS_MAC_PROVISION_BIN` is needed when the test binary runs on another machine than the
//! one that built it: the path cargo knew at build time is not there.
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
//! - a pool uid reaches the listed endpoint;
//! - a pool uid does not reach anything else: this Mac's own address on a port that is not
//!   listed, the metadata address and a documentation address. A block is proven by pf's
//!   own counter, not by the error the client sees: the packet counter of the blocking rule
//!   (`pfctl -a <anchor> -s rules -vv`, read before and after the probe) must rise, and the
//!   outcome must be "refused" or "timed out", never "connected" and never another error
//!   (a route error such as "host unreachable" means routing refused it, not pf). macOS pf
//!   builds the reset of a `block return` only for a packet whose checksum it can verify,
//!   and an outbound SYN does not have one yet on an interface with checksum assist (xnu
//!   `pf_test_rule` calling `pf_check_proto_cksum`), so a blocked TCP connect is dropped
//!   silently and times out; "refused" is accepted when a host does answer. A probe whose
//!   counters could not be read is "unproven", and that fails;
//! - the same uid reaches IPv4 loopback;
//! - a uid outside the pool is refused by nothing;
//! - after `remove` the pool uid is no longer refused.
//!
//! Every probe is recorded as a row: uid, target, outcome, the control uid's outcome to the
//! same target (a blocked target the control cannot reach either proves nothing by itself,
//! which is why the counter decides), the counter rise and what was expected. The table is
//! printed after the last probe and only then does the test fail, so one run on a Mac shows
//! every outcome and not just the first that differs.
//!
//! The file builds on every platform, so clippy reads it on Linux; run anywhere but macOS,
//! the test stops at its first line.

use std::fmt::Write as _;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_mac_provision::counters::{self, Which};
use js_mac_provision::pfctl::SystemPfctl;
use js_mac_provision::provision::Provisioner;
use js_mac_provision::{AnchorName, Endpoint, UidRange};

const BUILT_BIN: &str = env!("CARGO_BIN_EXE_js-mac-provision");
/// The tests' own directory, where every user may search.
const SCRATCH: &str = "/private/tmp/js-mac-provision-pf-as-root";
const ANCHOR: &str = "com.apple/js-mac-provision-test";
const POOL_FIRST: u32 = 4100;
const POOL_COUNT: u32 = 10;
const CONTROL_UID: u32 = 4200;
const METADATA: &str = "169.254.169.254:80";
const DOCUMENTATION: &str = "198.51.100.1:80";

/// The program the probes run: the one cargo built, or the one `JS_MAC_PROVISION_BIN` names.
fn program_to_copy() -> PathBuf {
    let named = std::env::var_os("JS_MAC_PROVISION_BIN");
    named.map_or_else(|| PathBuf::from(BUILT_BIN), PathBuf::from)
}

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
///
/// The 2000 ms connect bound is how a drop is observed as an outcome (a connect nobody
/// answers ends in "timed out"); it is a bound, not a measurement, and nothing here
/// asserts how long a probe took. The `probe` verb prints "timed out" for both
/// `ErrorKind::TimedOut` and `ErrorKind::WouldBlock`.
fn probe_as(program: &Path, uid: u32, addr: &str) -> String {
    let out = Command::new(program)
        .args(["probe", addr, "--timeout-ms", "2000"])
        .uid(uid)
        .gid(uid)
        .output()
        .unwrap_or_else(|e| panic!("start the probe as uid {uid} (is this root?): {e}"));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// What a probe is expected to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    /// The connection is made.
    Connected,
    /// pf blocked it: the blocking rule's counter rose, and the client saw "refused" or
    /// "timed out" (see the module docs for why a reset is not required).
    Blocked(Which),
    /// Anything but "refused" (a control uid: no rule matches it).
    NotRefused,
}

/// The block rule's packet counter around a probe.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Counter {
    /// Not measured: the row is not a blocked one.
    NotMeasured,
    /// The counters could not be read, before or after.
    Unread(String),
    /// How many more packets the blocking rule matched after the probe.
    Rose(u64),
}

/// What a row amounts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Ok,
    Fail,
    /// A block that cannot be told from a host that is absent, because the counter was not
    /// read. Not a pass.
    Unproven,
}

/// One probe: who connected where, what came of it, and what was expected.
struct Row {
    uid: u32,
    target: String,
    what: &'static str,
    got: String,
    want: Want,
    /// The control uid's outcome to the same target, for a blocked row.
    control: Option<String>,
    counter: Counter,
}

impl Row {
    fn verdict(&self) -> Verdict {
        let _ = (&self.got, self.want, &self.counter);
        Verdict::Fail
    }
}

#[allow(dead_code)]
fn verdict_of(ok: bool) -> Verdict {
    if ok { Verdict::Ok } else { Verdict::Fail }
}

/// What the test needs to probe and to read the counters.
struct Ctx<'a> {
    program: &'a Path,
    pf: &'a SystemPfctl,
    anchor: &'a AnchorName,
}

/// Runs the probe as `uid` and records it.
fn record(
    rows: &mut Vec<Row>,
    ctx: &Ctx<'_>,
    uid: u32,
    what: &'static str,
    target: &str,
    want: Want,
) {
    rows.push(Row {
        uid,
        target: target.to_owned(),
        what,
        got: probe_as(ctx.program, uid, target),
        want,
        control: None,
        counter: Counter::NotMeasured,
    });
}

/// Runs a probe expected to be blocked: the block rule's counters are read before and after,
/// and the control uid then probes the same target.
fn record_blocked(
    rows: &mut Vec<Row>,
    ctx: &Ctx<'_>,
    uid: u32,
    what: &'static str,
    target: &str,
    which: Which,
) {
    let before = counters::read(ctx.pf, ctx.anchor);
    let got = probe_as(ctx.program, uid, target);
    let after = counters::read(ctx.pf, ctx.anchor);
    let counter = match (before, after) {
        (Ok(b), Ok(a)) => Counter::Rose(counters::rise(&b, &a, uid, which)),
        (Err(e), _) | (_, Err(e)) => Counter::Unread(e.to_string()),
    };
    rows.push(Row {
        uid,
        target: target.to_owned(),
        what,
        got,
        want: Want::Blocked(which),
        control: Some(probe_as(ctx.program, CONTROL_UID, target)),
        counter,
    });
}

/// The rows that did not show what was expected, unproven ones included.
fn failed(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| r.verdict() != Verdict::Ok).collect()
}

/// Every row, one line each, the ones that differ from what was expected marked `FAIL` and
/// the ones that could not be proven `UNPROVEN`.
fn table(rows: &[Row]) -> String {
    let mut text = String::new();
    for r in rows {
        let mark = match r.verdict() {
            Verdict::Ok => "ok",
            Verdict::Fail => "FAIL",
            Verdict::Unproven => "UNPROVEN",
        };
        let control = r.control.as_deref().unwrap_or("-");
        let counter = match &r.counter {
            Counter::NotMeasured => "-".to_owned(),
            Counter::Rose(n) => format!("+{n}"),
            Counter::Unread(why) => format!("unread ({why})"),
        };
        // Writing to a String cannot fail.
        let _ = writeln!(
            text,
            "{mark:<8} uid {:<5} {:<22} {:<24} got {:<12} control {control:<12} counter {counter:<6} expected {:?}",
            r.uid, r.target, r.what, r.got, r.want
        );
    }
    text
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
    fs::copy(program_to_copy(), &program).expect("copy the program");
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

    let ctx = Ctx {
        program: &program,
        pf: &pf,
        anchor: &anchor,
    };
    let mut rows = Vec::new();

    // The pool: the first and the last uid of the range.
    for uid in [POOL_FIRST, POOL_FIRST + POOL_COUNT - 1] {
        record(&mut rows, &ctx, uid, "listed", &allowed_at, Want::Connected);
        // Reachable by construction (the control connects): the counter has a real case.
        record_blocked(
            &mut rows,
            &ctx,
            uid,
            "own host, other port",
            &unlisted_at,
            Which::CatchAll,
        );
        record_blocked(
            &mut rows,
            &ctx,
            uid,
            "metadata address",
            METADATA,
            Which::LinkLocal,
        );
        record_blocked(
            &mut rows,
            &ctx,
            uid,
            "unlisted address",
            DOCUMENTATION,
            Which::CatchAll,
        );
        record(
            &mut rows,
            &ctx,
            uid,
            "loopback",
            &loopback_at,
            Want::Connected,
        );
    }
    // The uids either side of the range: the rules name the uids 4100 to 4109 and no more.
    for uid in [POOL_FIRST - 1, POOL_FIRST + POOL_COUNT] {
        record(
            &mut rows,
            &ctx,
            uid,
            "outside the range",
            &unlisted_at,
            Want::Connected,
        );
    }

    // The control: a uid outside the pool is not affected. These would also hold with no
    // anchor at all, which is the point: pf changed nothing for them. The metadata and
    // documentation addresses may time out or be unreachable (the Mac has no such host),
    // but no rule answers them.
    record(
        &mut rows,
        &ctx,
        CONTROL_UID,
        "control, listed",
        &allowed_at,
        Want::Connected,
    );
    record(
        &mut rows,
        &ctx,
        CONTROL_UID,
        "control, own host",
        &unlisted_at,
        Want::Connected,
    );
    record(
        &mut rows,
        &ctx,
        CONTROL_UID,
        "control, metadata",
        METADATA,
        Want::NotRefused,
    );
    record(
        &mut rows,
        &ctx,
        CONTROL_UID,
        "control, documentation",
        DOCUMENTATION,
        Want::NotRefused,
    );

    // Remove flushes the anchor: the pool uid is reaching the listener again.
    provisioner.remove().expect("remove the anchor");
    record(
        &mut rows,
        &ctx,
        POOL_FIRST,
        "after remove",
        &unlisted_at,
        Want::Connected,
    );

    // Everything is printed before anything is judged.
    println!("{}", table(&rows));
    let bad = failed(&rows);
    assert!(
        bad.is_empty(),
        "{} of {} probes did not show what was expected (the table above has every one)",
        bad.len(),
        rows.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(want: Want, got: &str, counter: Counter) -> Row {
        Row {
            uid: 4100,
            target: "192.0.2.1:80".to_owned(),
            what: "a probe",
            got: got.to_owned(),
            want,
            control: Some("host unreachable".to_owned()),
            counter,
        }
    }

    const BLOCKED: Want = Want::Blocked(Which::CatchAll);

    #[test]
    fn a_block_needs_the_counter_to_rise() {
        assert_eq!(
            row(BLOCKED, "timed out", Counter::Rose(1)).verdict(),
            Verdict::Ok
        );
        assert_eq!(
            row(BLOCKED, "refused", Counter::Rose(3)).verdict(),
            Verdict::Ok
        );
        // The error alone proves nothing: pf matched no packet.
        assert_eq!(
            row(BLOCKED, "timed out", Counter::Rose(0)).verdict(),
            Verdict::Fail
        );
        assert_eq!(
            row(BLOCKED, "refused", Counter::Rose(0)).verdict(),
            Verdict::Fail
        );
    }

    #[test]
    fn a_block_is_never_a_connection_or_another_error() {
        for got in ["connected", "failed: host unreachable", "failed: other"] {
            assert_eq!(
                row(BLOCKED, got, Counter::Rose(5)).verdict(),
                Verdict::Fail,
                "{got}"
            );
        }
    }

    #[test]
    fn a_block_whose_counter_was_not_read_is_unproven_not_passed() {
        let unread = Counter::Unread("pfctl failed".to_owned());
        assert_eq!(
            row(BLOCKED, "timed out", unread.clone()).verdict(),
            Verdict::Unproven
        );
        assert_eq!(
            row(BLOCKED, "timed out", Counter::NotMeasured).verdict(),
            Verdict::Unproven
        );
        // A wrong outcome is a failure whether or not the counter was read.
        assert_eq!(row(BLOCKED, "connected", unread).verdict(), Verdict::Fail);
    }

    #[test]
    fn connected_and_not_refused_mean_what_they_say() {
        let n = Counter::NotMeasured;
        assert_eq!(
            row(Want::Connected, "connected", n.clone()).verdict(),
            Verdict::Ok
        );
        assert_eq!(
            row(Want::Connected, "timed out", n.clone()).verdict(),
            Verdict::Fail
        );
        assert_eq!(
            row(Want::NotRefused, "timed out", n.clone()).verdict(),
            Verdict::Ok
        );
        assert_eq!(row(Want::NotRefused, "refused", n).verdict(), Verdict::Fail);
    }

    #[test]
    fn the_table_lists_every_row_with_its_control_and_counter() {
        let rows = vec![
            row(BLOCKED, "timed out", Counter::Rose(2)),
            row(BLOCKED, "timed out", Counter::Rose(0)),
            row(BLOCKED, "timed out", Counter::Unread("no pfctl".to_owned())),
            row(Want::Connected, "connected", Counter::NotMeasured),
        ];
        let text = table(&rows);
        assert_eq!(text.lines().count(), 4);
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with("ok "));
        assert!(first.contains("control host unreachable"));
        assert!(first.contains("counter +2"));
        assert!(lines.next().unwrap().starts_with("FAIL"));
        let third = lines.next().unwrap();
        assert!(third.starts_with("UNPROVEN"));
        assert!(third.contains("unread (no pfctl)"));
        assert!(lines.next().unwrap().contains("counter -"));
    }

    #[test]
    fn failed_keeps_every_row_that_is_not_ok() {
        let rows = vec![
            row(BLOCKED, "timed out", Counter::Rose(1)),
            row(BLOCKED, "timed out", Counter::Rose(0)),
            row(BLOCKED, "timed out", Counter::NotMeasured),
            row(Want::Connected, "refused", Counter::NotMeasured),
        ];
        assert_eq!(failed(&rows).len(), 3);
    }
}
