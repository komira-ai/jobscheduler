//! A native job on loopback, for real, as root: a job of the level `none` on an agent whose
//! ceiling is `loopback` runs as a pool account under its Seatbelt profile, started by the real
//! supervisor; it listens on `127.0.0.1` and `::1` (port 0 and a fixed port) and reaches its own
//! listener, but reaches no listener of a host service (a stand-in, run as another user), cannot
//! take a host port (`--native-host-port`) while nothing holds it, and reaches no address off
//! loopback. The runtime reads the Mac's real listeners (`mac_listen::MacListeners`).
//!
//! A job here makes eight tries of each step and prints how many worked and the errno of each
//! that did not. As in js-mac-pool's runner tests, the sandbox answers about one allowed
//! loopback connect in 400 with `EPERM` on a hosted runner, so an allowed step needs seven of
//! eight with every failure `EPERM`, and a refused step needs all eight refused with `EPERM`.

use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use js_agent::lifecycle::{Body, Runtime};
use js_agent::native::{Ceiling, NativeRuntime};
use js_agent_wire::NetworkLevel;

use super::{exited, fixture, runtime_with, settle, spec};

/// The uid of the stand-in host service: outside the pool's slots (4300 to 4309), and no
/// account needs to hold it.
const SERVICE_UID: u32 = 4323;
/// The group the stand-in runs in (`staff`).
const STAFF: u32 = 20;

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

/// A runtime whose ceiling is `loopback` and whose host ports are `host_ports`.
fn loopback_runtime(host_ports: &[u16]) -> NativeRuntime {
    runtime_with(fixture(), |config| {
        config.network = Ceiling::Loopback;
        config.host_ports = host_ports.iter().copied().collect();
    })
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

/// A stand-in host service: a listener of [`SERVICE_UID`] on 127.0.0.1 and on ::1, each at a
/// port of its own, killed when dropped.
struct Service {
    child: Child,
    v4: u16,
    v6: u16,
}

impl Service {
    fn start() -> Self {
        let script = r#"use IO::Socket::IP; $| = 1;
my @s = map { IO::Socket::IP->new(Listen => 8, LocalHost => $_, LocalPort => 0) or die "$_: $!" } ("127.0.0.1", "::1");
print join(" ", map { $_->sockport } @s), "\n";
while (1) { for my $s (@s) { $s->blocking(0); my $c = $s->accept; } select(undef, undef, undef, 0.05) }"#;
        let mut child = Command::new("/usr/bin/perl")
            .args(["-e", script])
            .uid(SERVICE_UID)
            .gid(STAFF)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ports: Vec<u16> = line
            .split_whitespace()
            .map(|port| port.parse().unwrap())
            .collect();
        assert_eq!(ports.len(), 2, "the stand-in's ports: {line:?}");
        Self {
            child,
            v4: ports[0],
            v6: ports[1],
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Asserts that a job of the level `none` under the ceiling `loopback` listens on
/// 127.0.0.1:0 and ::1:0 and reaches its own listener on each, and listens on a fixed free
/// port of each. Before jobs of the level `none` had loopback, every one of these steps was
/// refused. Catches a Mac build job that cannot run the loopback tests a Linux job runs.
#[test]
#[ignore = "needs root and creates users"]
fn a_none_job_listens_on_both_loopbacks_and_reaches_itself() {
    let mut rt = loopback_runtime(&[]);
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

/// Asserts that a job reaches no listener of a host service, run as another user on
/// 127.0.0.1 and ::1 at ports no flag names (the runtime finds them when the run starts),
/// whether it asks for no network or for `loopback`, while the test (root, outside the
/// sandbox) reaches both. A job that asked for `loopback` reached them before the host's
/// ports were denied. Catches a host service on loopback left reachable from a job, an IPv6
/// listener missed, and a listing that skips another user's processes.
#[test]
#[ignore = "needs root and creates users"]
fn a_job_reaches_no_host_service_on_loopback() {
    let service = Service::start();
    for (host, port) in [("127.0.0.1", service.v4), ("[::1]", service.v6)] {
        let at = format!("{host}:{port}");
        std::net::TcpStream::connect(&at).unwrap_or_else(|why| panic!("control {at}: {why}"));
    }
    let mut rt = loopback_runtime(&[]);
    let levels = [NetworkLevel::Off, NetworkLevel::Loopback];
    for (n, level) in levels.into_iter().enumerate() {
        for (host, port) in [("127.0.0.1", service.v4), ("::1", service.v6)] {
            let port = port.to_string();
            probe(
                &mut rt,
                &format!("lo-service{n}-{port}"),
                level,
                &["refused", "connect", host, &port],
            );
        }
    }
}

/// Asserts that a job cannot listen on a port of `--native-host-port` while nothing holds it,
/// on 127.0.0.1, ::1 or the wildcard, and still listens on another free port. Catches a host
/// port denied only while its service listens: a job would take the port of the tree mount's
/// file service while it restarts, and the Mac's client would read the job's answers.
#[test]
#[ignore = "needs root and creates users"]
fn a_job_cannot_take_a_host_port_while_its_service_is_down() {
    let host_port = free_port();
    let mut rt = loopback_runtime(&[host_port]);
    let port = host_port.to_string();
    for (n, host) in ["127.0.0.1", "::1", "0.0.0.0"].into_iter().enumerate() {
        probe(
            &mut rt,
            &format!("lo-take{n}"),
            NetworkLevel::Off,
            &["refused", "listen", host, &port],
        );
    }
    let other = free_port().to_string();
    probe(
        &mut rt,
        "lo-other",
        NetworkLevel::Off,
        &["allowed", "listen", "127.0.0.1", &other],
    );
}

/// Asserts that a job of the level `none` reaches no address off loopback: a connect to
/// 192.0.2.1, an address kept for documentation that no host holds, is refused by the sandbox
/// with `EPERM` (not a timeout or an unreachable network, which a connect that left the Mac
/// would give). Catches loopback granted as the whole network.
#[test]
#[ignore = "needs root and creates users"]
fn a_none_job_reaches_no_address_off_loopback() {
    let mut rt = loopback_runtime(&[]);
    probe(
        &mut rt,
        "lo-remote",
        NetworkLevel::Off,
        &["refused", "connect", "192.0.2.1", "9"],
    );
}
