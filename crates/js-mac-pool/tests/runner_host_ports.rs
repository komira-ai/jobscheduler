//! The host's ports under the real `sandbox-exec` on a macOS runner, as the runner's own
//! user: a run binds and connects on loopback, IPv4 and IPv6, but neither binds nor reaches a
//! denied port, and reaches no address off loopback. Each refusal has a control: the same
//! step without the sandbox succeeds, or the refusal proves nothing.
//!
//! The listeners here are the test's own (the runner's user); the profile denies by port, not
//! by owner, so whose listener it is does not change what the profile does. The root tests of
//! `js-agent` (`tests/native_privileged/loopback.rs`) run the same steps as a pool account
//! against a listener of another user.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use js_mac_pool::seatbelt::{
    AbsPath, Caps, Narrowing, Net, Policy, Ports, Profile, ReadModel, SlotLayout,
};

/// Connects to `HOST PORT` and exits 0 when it connects; otherwise prints the errno on stderr
/// and exits 1. A sandbox refusal is `EPERM` (1), a closed port `ECONNREFUSED`, so a refusal
/// is told apart from a listener that is not there. `IO::Socket::IP` takes IPv6 as well.
const CONNECT: &str = r#"use IO::Socket::IP; my $s = IO::Socket::IP->new(PeerHost => $ARGV[0], PeerPort => $ARGV[1], Timeout => 5); exit 0 if $s; print STDERR ($!+0); exit 1"#;

/// Listens on `HOST PORT` (port 0 is any free port) and exits 0 when it can; otherwise prints
/// the errno and exits 1.
const LISTEN: &str = r#"use IO::Socket::IP; my $s = IO::Socket::IP->new(Listen => 1, LocalHost => $ARGV[0], LocalPort => $ARGV[1]); exit 0 if $s; print STDERR ($!+0); exit 1"#;

/// Listens on `HOST:0`, connects to its own listener and accepts the connection: exits 0
/// when all three work, otherwise prints the errno and exits 1.
const OWN: &str = r#"use IO::Socket::IP; my $l = IO::Socket::IP->new(Listen => 1, LocalHost => $ARGV[0], LocalPort => 0) or do { print STDERR ($!+0); exit 1 }; my $c = IO::Socket::IP->new(PeerHost => $ARGV[0], PeerPort => $l->sockport) or do { print STDERR ($!+0); exit 1 }; $l->accept or do { print STDERR ($!+0); exit 1 }; exit 0"#;

const EPERM: &str = "1";

/// How many times each step is tried, and how many of an allowed step must succeed: as in
/// `tests/runner.rs`, the sandbox answers about one allowed loopback connect in 400 with
/// `EPERM` on a hosted runner, so an allow tolerates one such failure and nothing else, and a
/// refusal tolerates none.
const TRIES: usize = 8;
const ALLOW_MIN: usize = 7;

/// A run directory under the build's own target directory, removed when dropped.
struct Scene {
    base: PathBuf,
    run: PathBuf,
}

impl Scene {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let base = root.join(format!("mac-pool-ports-{tag}-{}", std::process::id()));
        let run = base.join("run");
        std::fs::create_dir_all(&run).expect("run directory");
        Self {
            base: base.canonicalize().expect("base"),
            run: run.canonicalize().expect("run"),
        }
    }

    /// The policy of `net` that denies `ports`.
    fn policy(&self, net: Net, ports: &[u16]) -> Policy {
        let layout = SlotLayout {
            trees: None,
            run: AbsPath::new(&self.run).expect("run path"),
            uid_entry: None,
            inputs: vec![],
            read_denies: vec![],
            mounts: vec![],
        };
        let caps = Caps {
            net,
            read_model: ReadModel::Open,
        };
        Policy::base(layout, caps)
            .and_then(|policy| {
                policy.narrowed(Narrowing {
                    deny_ports: ports.to_vec(),
                    ..Narrowing::default()
                })
            })
            .expect("policy")
    }

    fn perl(&self, policy: &Policy, script: &str, rest: &[String]) -> Output {
        let profile = Profile::for_policy(policy).expect("profile");
        let argv = profile.argv(
            AbsPath::parse("/usr/bin/perl").expect("program"),
            &perl_args(script, rest),
        );
        Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("sandbox-exec starts")
    }

    fn allowed(&self, policy: &Policy, script: &str, rest: &[String]) -> bool {
        let mut done = 0;
        for _ in 0..TRIES {
            let out = self.perl(policy, script, rest);
            if out.status.success() {
                done += 1;
            } else if String::from_utf8_lossy(&out.stderr).trim() != EPERM {
                return false;
            }
        }
        done >= ALLOW_MIN
    }

    fn refused(&self, policy: &Policy, script: &str, rest: &[String]) -> bool {
        (0..TRIES).all(|_| {
            let out = self.perl(policy, script, rest);
            !out.status.success() && String::from_utf8_lossy(&out.stderr).trim() == EPERM
        })
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Best effort: the runner is discarded anyway.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn perl_args(script: &str, rest: &[String]) -> Vec<OsString> {
    let mut all = vec![OsString::from("-e"), OsString::from(script)];
    all.extend(rest.iter().map(OsString::from));
    all
}

fn control(script: &str, rest: &[String]) -> Output {
    Command::new("/usr/bin/perl")
        .args(perl_args(script, rest))
        .output()
        .expect("control starts")
}

fn describe(out: &Output) -> String {
    format!("{:?} {}", out.status, String::from_utf8_lossy(&out.stderr))
}

/// A listener on `host:0` that accepts every connection on its own thread, so a connect
/// completes by the handshake and never waits in an accept queue.
fn accepting(host: &str) -> u16 {
    let at: SocketAddr = format!("{host}:0").parse().expect("address");
    let listener = TcpListener::bind(at).expect("listener");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || for _connection in listener.incoming() {});
    port
}

/// A port nothing listens on: one the system gave a listener that is closed again.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.local_addr().expect("addr").port()
}

fn to(host: &str, port: u16) -> Vec<String> {
    vec![host.to_owned(), port.to_string()]
}

/// Asserts that under `Loopback` and under `On`, with one host port denied, a connect to that
/// port is refused on 127.0.0.1 and on ::1 while a connect to another loopback listener
/// succeeds, and that both connects succeed without the sandbox. Catches a deny that misses
/// IPv6 loopback, a deny that closes all of loopback, and a level (`On`, which has no network
/// rule of its own) that leaves the host's port reachable.
#[test]
fn a_denied_port_is_refused_on_both_loopbacks_and_other_ports_connect() {
    let scene = Scene::new("connect");
    for host in ["127.0.0.1", "::1"] {
        let host_service = accepting(host);
        let other = accepting(host);
        for port in [host_service, other] {
            let out = control(CONNECT, &to(host, port));
            assert!(
                out.status.success(),
                "control {host} {port}: {}",
                describe(&out)
            );
        }
        for net in [Net::Loopback, Net::On] {
            let policy = scene.policy(net.clone(), &[host_service]);
            assert!(
                scene.refused(&policy, CONNECT, &to(host, host_service)),
                "{net:?}: the denied port {host} {host_service} is refused"
            );
            assert!(
                scene.allowed(&policy, CONNECT, &to(host, other)),
                "{net:?}: another loopback port {host} {other} connects"
            );
        }
    }
}

/// Asserts that under `Loopback` a run listens on 127.0.0.1:0 and ::1:0 and connects to its own
/// listener on each, and that a free denied port can be listened on without the sandbox but
/// not under it, on 127.0.0.1, ::1, and (under `On`) the wildcard address. Catches a profile
/// that refuses the loopback listeners the tests of a build need, and one that lets a run take
/// a host service's port while the service is down, where the service's clients would reach
/// the run instead.
#[test]
fn a_run_listens_on_loopback_and_reaches_itself_but_cannot_take_a_denied_port() {
    let scene = Scene::new("listen");
    let denied = free_port();
    let policy = scene.policy(Net::Loopback, &[denied]);
    for host in ["127.0.0.1", "::1"] {
        assert!(
            scene.allowed(&policy, LISTEN, &to(host, 0)),
            "Loopback listens on {host}:0"
        );
        assert!(
            scene.allowed(&policy, OWN, &[host.to_owned()]),
            "Loopback reaches its own listener on {host}"
        );
        let out = control(LISTEN, &to(host, denied));
        assert!(
            out.status.success(),
            "control listen {host} {denied}: {}",
            describe(&out)
        );
        assert!(
            scene.refused(&policy, LISTEN, &to(host, denied)),
            "Loopback refuses a listener on the denied {host} {denied}"
        );
    }
    let on = scene.policy(Net::On, &[denied]);
    let out = control(LISTEN, &to("0.0.0.0", denied));
    assert!(
        out.status.success(),
        "control listen *:{denied}: {}",
        describe(&out)
    );
    assert!(
        scene.refused(&on, LISTEN, &to("0.0.0.0", denied)),
        "On refuses a listener on the denied *:{denied}"
    );
    assert!(
        scene.allowed(&on, LISTEN, &to("0.0.0.0", 0)),
        "On still listens on *:0"
    );
}

/// Asserts that under `Loopback`, with host ports denied, a connect to an address off loopback
/// (192.0.2.1, an address kept for documentation that no host holds) is refused by the
/// sandbox, as a listed remote port is under `Ports`. Catches a deny of the host's ports that
/// widens what else the level reaches.
#[test]
fn loopback_with_denied_ports_reaches_no_other_address() {
    let scene = Scene::new("remote");
    let denied = free_port();
    let only = Net::Ports(Ports::new(&[9]).expect("ports"));
    for net in [Net::Loopback, only] {
        let policy = scene.policy(net.clone(), &[denied]);
        assert!(
            scene.refused(&policy, CONNECT, &to("192.0.2.1", 9)),
            "{net:?} refuses 192.0.2.1"
        );
    }
}
