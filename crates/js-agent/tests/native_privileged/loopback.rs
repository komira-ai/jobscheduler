//! A native job on loopback, for real, as root: a job of the level `none` on an agent whose
//! ceiling is `loopback` runs as a pool account under its Seatbelt profile, started by the real
//! supervisor; it listens on `127.0.0.1` and `::1` (port 0 and a fixed port) and reaches its own
//! listener, and reaches no address off loopback.
//!
//! A job here makes eight tries of each step and prints how many worked and the errno of each
//! that did not. As in js-mac-pool's runner tests, the sandbox answers about one allowed
//! loopback connect in 400 with `EPERM` on a hosted runner, so an allowed step needs seven of
//! eight with every failure `EPERM`, and a refused step needs all eight refused with `EPERM`.

use std::net::TcpListener;

use js_agent::lifecycle::{Body, Runtime};
use js_agent::native::{Ceiling, NativeRuntime};
use js_agent_wire::NetworkLevel;

use super::{exited, fixture, runtime_with, settle, spec};

/// The job: `WANT WHAT HOST [PORT]`. `WHAT` is `own` (listen on `HOST:0`, connect to that
/// listener and accept), `listen` (listen on `HOST:PORT`) or `connect` (to `HOST:PORT`).
/// `WANT` is `allowed` (exit 0 when seven of eight tries work and every failure is `EPERM`) or
/// `refused` (exit 0 when all eight fail with `EPERM`). It prints the count and the errnos.
const PROBE: &str = r#"use IO::Socket::IP;
my ($want, $what, $host, $port) = @ARGV;
sub once {
    if ($what eq "own") {
        my $l = IO::Socket::IP->new(Listen => 1, LocalHost => $host, LocalPort => 0) or return $! + 0;
        my $c = IO::Socket::IP->new(PeerHost => $host, PeerPort => $l->sockport) or return $! + 0;
        $l->accept or return $! + 0;
        return 0;
    }
    if ($what eq "listen") {
        IO::Socket::IP->new(Listen => 1, LocalHost => $host, LocalPort => $port, ReuseAddr => 1) or return $! + 0;
        return 0;
    }
    IO::Socket::IP->new(PeerHost => $host, PeerPort => $port, Timeout => 5) or return $! + 0;
    return 0;
}
my ($ok, @errs) = (0);
for (1 .. 8) { my $e = once(); if ($e == 0) { $ok++ } else { push @errs, $e } }
print "$ok @errs\n";
my $other = grep { $_ != 1 } @errs;
if ($want eq "allowed") { exit($other ? 2 : ($ok >= 7 ? 0 : 1)) }
exit(($ok == 0 && !$other) ? 0 : 1);
"#;

/// A runtime whose ceiling is `loopback`.
fn loopback_runtime() -> NativeRuntime {
    runtime_with(fixture(), |config| config.network = Ceiling::Loopback)
}

/// Runs the probe `args` as a job at `level` and asserts that it exited 0, with what it printed.
fn probe(rt: &mut NativeRuntime, run_id: &str, level: NetworkLevel, args: &[&str]) {
    let all = [&["-e", PROBE][..], args].concat();
    let mut run = spec(fixture(), run_id, "/usr/bin/perl", &all);
    let Body::Native(body) = &mut run.body else {
        panic!("a native body");
    };
    body.network = level;
    let id = rt.create_and_start(&run).unwrap();
    let status = settle(rt, &id);
    let tail = rt.log_tail(&id).unwrap().unwrap();
    rt.remove(&id).unwrap();
    let said = format!(
        "{run_id} {args:?}: {} {}",
        tail.stdout.trim(),
        tail.stderr.trim()
    );
    assert_eq!(status, exited(0), "{said}");
}

/// A port nothing listens on: one the system gave a listener that is closed again.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Asserts that a job of the level `none` under the ceiling `loopback` listens on
/// 127.0.0.1:0 and ::1:0 and reaches its own listener on each, and listens on a fixed free
/// port of each. Before jobs of the level `none` had loopback, every one of these steps was
/// refused. Catches a Mac build job that cannot run the loopback tests a Linux job runs.
#[test]
#[ignore = "needs root and creates users"]
fn a_none_job_listens_on_both_loopbacks_and_reaches_itself() {
    let mut rt = loopback_runtime();
    let fixed = free_port().to_string();
    for (n, host) in ["127.0.0.1", "::1"].into_iter().enumerate() {
        probe(
            &mut rt,
            &format!("lo-own{n}"),
            NetworkLevel::Off,
            &["allowed", "own", host],
        );
        probe(
            &mut rt,
            &format!("lo-fixed{n}"),
            NetworkLevel::Off,
            &["allowed", "listen", host, &fixed],
        );
    }
}

/// Asserts that a job of the level `none` reaches no address off loopback: a connect to
/// 192.0.2.1, an address kept for documentation that no host holds, is refused by the sandbox
/// with `EPERM` (not a timeout or an unreachable network, which a connect that left the Mac
/// would give). Catches loopback granted as the whole network.
#[test]
#[ignore = "needs root and creates users"]
fn a_none_job_reaches_no_address_off_loopback() {
    let mut rt = loopback_runtime();
    probe(
        &mut rt,
        "lo-remote",
        NetworkLevel::Off,
        &["refused", "connect", "192.0.2.1", "9"],
    );
}
