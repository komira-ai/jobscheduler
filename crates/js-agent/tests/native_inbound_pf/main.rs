//! Does the real `js-mac-provision` pf anchor filter INBOUND traffic to a listener owned by a
//! pool uid? Run for real, as root, on a Mac.
//!
//! Why it is asked: today the agent refuses a job's declared ports on macOS
//! (`ports-not-supported`). The anchor holds only `out` rules, each with `user`, and its last
//! rule is `block return out quick proto { tcp, udp } from any to any user <range>`, with
//! loopback passed. A listener's SYN-ACK is an outbound packet of the pool uid's socket, so the
//! anchor may or may not stop a connection from outside to a listener the pool uid owns. This
//! test measures it. A pass would allow a later change to accept declared ports on a Mac; this
//! change does not, and the refusal stands until the test passes.
//!
//! What it does, with the real `js-mac-session` helper served from this process on a range of
//! ONE uid (as `native_isolation` does):
//!
//! 1. A native run through the agent's own `NativeRuntime` (network cap `On`, the run's level
//!    `On`) executes this binary in the role `listen`: it prints its `getuid()`, binds a TCP
//!    listener on 0.0.0.0 port 0, prints the port, and answers every connection with one byte.
//!    The test reads those lines from the run's stdout log with a bounded count of polls and
//!    checks that the uid is the configured one.
//! 2. The test installs the real anchor for that one uid with no endpoints, under its own
//!    test anchor and its own state directory, through `Provisioner` and `SystemPfctl`. The
//!    runtime is opened ungated: the test installs and removes the anchor itself, so the
//!    runtime's own check of the anchor is not needed.
//! 3. Three connects from the test process (root), each with a connect timeout that only
//!    bounds the probe:
//!    - case a, to this Mac's primary IPv4 address with the anchor in force. Expected: not
//!      reachable. A block counts as proven only when the connect did not succeed AND the
//!      packet counter of the anchor's blocking rule rose around it (pf drops a blocked SYN
//!      silently, so the client only sees a timeout). Not connected with no rise is unproven;
//!    - case b, a control to 127.0.0.1 with the anchor in force. Expected: reachable, which
//!      shows the listener is up;
//!    - case c, a control to the primary address after the anchor is removed. Expected:
//!      reachable, which tells the filter from the absence of a listener.
//! 4. One line per case (`case a: ...`) and then exactly one final line:
//!    `inbound-by-uid: filtered` (a proven blocked, b and c reachable),
//!    `inbound-by-uid: NOT filtered` (a connected), or `inbound-by-uid: could not tell: ...`
//!    (b or c failed, or a is unproven). Only `filtered` passes; the others panic, so the
//!    process exits non-zero. The run is ended and the anchor removed whatever happened.
//!
//! Nothing asserts on elapsed time: the counter decides a block.
//!
//! No pull request job runs it (rule B9). It needs root, a Mac and a free uid, makes a user,
//! loads pf rules under its own test anchor and removes them: never run it on a production
//! host. The manual workflow `js-agent-native-inbound-pf-test` builds the binary for Apple
//! silicon and uploads it without running it. Run it as root on a provisioned Mac with no
//! other use, with `JS_NATIVE_INBOUND_ROOT` naming a new directory, owned by root and mode 0755,
//! that every user can search down to, and `JS_NATIVE_INBOUND_UID` naming a uid in 500 to
//! 60000 that nothing uses (test-binary parameters for the operator), with `--ignored
//! --test-threads=1 --nocapture`.
//!
//! One difference from production: the runtime here has the directory of this test binary as
//! an extra tool root (the run executes the binary).
//!
//! The file builds on every platform, so clippy reads it on Linux; the pure part is tested
//! there. Run anywhere but macOS, the ignored test stops at its first line.

#[cfg(target_os = "macos")]
mod mac;
mod verdict;

use std::io::Write as _;
use std::net::TcpListener;

use verdict::{Verdict, final_line};

/// The environment name of the child's role; it does not start with a name the agent reserves.
const ROLE: &str = "INBOUND_ROLE";

/// What a run does when the test starts this binary as it: nothing, unless the role says.
/// Not `#[ignore]`: it is a no-op without the role, and the run names it exactly.
#[test]
fn child_of_the_runs() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    match role.as_str() {
        "listen" => listen(),
        other => panic!("unknown role {other:?}"),
    }
}

/// The `listen` role: prints the uid and the port, then answers every connection with one
/// byte and closes it, until the run is ended from outside.
fn listen() {
    println!("{}", verdict::uid_line(rustix::process::getuid().as_raw()));
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind a TCP listener");
    let port = listener.local_addr().expect("listener address").port();
    println!("{}", verdict::port_line(port));
    for mut stream in listener.incoming().flatten() {
        let _ = stream.write_all(b"x");
    }
}

/// Asserts that, with the real anchor loaded for the uid, a connection from outside to a
/// listener the uid owns on this Mac's primary address does not get through, proven by the
/// blocking rule's counter, while 127.0.0.1 and the primary address without the anchor do
/// connect. It fails when the connection gets through (the anchor does not filter inbound
/// traffic) and when the cases cannot tell (a control failed or the block is unproven).
#[test]
#[ignore = "needs root and a Mac; creates a user and loads pf rules under its own anchor"]
fn inbound_by_uid() {
    let verdict = run_on_a_mac();
    assert_eq!(verdict, Verdict::Filtered, "{}", final_line(&verdict));
}

#[cfg(target_os = "macos")]
fn run_on_a_mac() -> Verdict {
    mac::run()
}

#[cfg(not(target_os = "macos"))]
fn run_on_a_mac() -> Verdict {
    panic!("this test runs on macOS only")
}
