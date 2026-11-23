//! The listener table on macOS, read for real as root: listeners another user holds, found
//! with their address, port and uid, and launchd's own on-demand sockets.
//!
//! Every test needs root, so each is `#[ignore]` with that reason: the ordinary test run lists
//! them as ignored, by name, and runs none. beta.yml's macOS job runs exactly them as root
//! (`sudo ... cargo test -p js-agent --lib -- --ignored mac_listen::macos::root_tests`). A run
//! without root fails at its first line, saying so: it never passes without having run.

use std::io::{BufRead as _, BufReader};
use std::net::IpAddr;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use super::super::{Listener, ListenerTable, host_listeners};
use super::MacListeners;
use crate::mac_accounts::Slots;

/// The listener's uid: no account of the runner has it, and it lies in the range a pool's
/// slots may take (`Slots::HIGHEST` is 60000), so a slot range can hold it.
const OTHER: u32 = 58_125;
/// The group the listener runs in (`staff`).
const STAFF: u32 = 20;

/// Listens on each address of the arguments at port 0, prints the ports in the same order on
/// one line, and waits to be killed.
const LISTEN: &str = r#"use IO::Socket::IP; $| = 1; my @s = map { IO::Socket::IP->new(Listen => 1, LocalHost => $_, LocalPort => 0) or die "$_: $!" } @ARGV; print join(" ", map { $_->sockport } @s), "\n"; sleep 600"#;

fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "this test needs root on macOS: beta.yml runs it with sudo"
    );
}

/// A listener of [`OTHER`] on each of `hosts`, and its ports.
struct Stand {
    child: Child,
    ports: Vec<u16>,
}

impl Stand {
    fn on(hosts: &[&str]) -> Self {
        let mut child = Command::new("/usr/bin/perl")
            .arg("-e")
            .arg(LISTEN)
            .args(hosts)
            .uid(OTHER)
            .gid(STAFF)
            .stdout(Stdio::piped())
            .spawn()
            .expect("the listener starts");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("the listener's ports");
        let ports = line
            .split_whitespace()
            .map(|port| port.parse().expect("a port"))
            .collect::<Vec<u16>>();
        assert_eq!(ports.len(), hosts.len(), "ports: {line:?}");
        Self { child, ports }
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn of(found: &[Listener], pid: i32) -> Vec<(IpAddr, u16, u32, u32)> {
    let mut own: Vec<_> = found
        .iter()
        .filter(|listener| listener.pid == pid)
        .map(|listener| (listener.addr, listener.port, listener.uid, listener.ruid))
        .collect();
    own.sort();
    own
}

/// Asserts that the listing, as root, reads every process of the Mac without an error and
/// finds the listeners another user holds on 127.0.0.1, ::1 and the wildcard, each with its
/// own address, port and uid, and that the host listeners of the listing keep them while a
/// slot range holding their uid drops them. Catches a struct layout off by a field (a wrong
/// port or address), the port read in host byte order, a process of another user skipped, and
/// a listing that fails on some process of a stock Mac.
#[test]
#[ignore = "needs root: run by beta.yml's macOS job with sudo"]
fn the_listing_finds_another_users_listeners_with_their_address_port_and_uid() {
    require_root();
    let stand = Stand::on(&["127.0.0.1", "::1", "0.0.0.0"]);
    let pid = i32::try_from(stand.child.id()).expect("pid");
    let found = MacListeners.listeners().expect("the listing");
    let mut wanted: Vec<(IpAddr, u16, u32, u32)> = vec![
        ("127.0.0.1".parse().unwrap(), stand.ports[0], OTHER, OTHER),
        ("::1".parse().unwrap(), stand.ports[1], OTHER, OTHER),
        ("0.0.0.0".parse().unwrap(), stand.ports[2], OTHER, OTHER),
    ];
    wanted.sort();
    assert_eq!(of(&found, pid), wanted);

    let elsewhere = Slots::new(58_000, 10).expect("slots");
    let host = host_listeners(found.clone(), elsewhere);
    assert_eq!(
        of(&host, pid),
        wanted,
        "another user's listeners are the host's"
    );
    let holding = Slots::new(OTHER, 1).expect("slots");
    let host = host_listeners(found, holding);
    assert_eq!(of(&host, pid), Vec::new(), "a slot's listeners are not");
}

/// Asserts that a listener closed before the listing is not found, so the listing is of the
/// moment it runs. Catches a listing that reports a stale socket, which would deny a port no
/// one holds any more (harmless) or, read the other way, a table cached at the agent's start.
#[test]
#[ignore = "needs root: run by beta.yml's macOS job with sudo"]
fn a_listener_closed_before_the_listing_is_not_found() {
    require_root();
    let stand = Stand::on(&["127.0.0.1"]);
    let pid = i32::try_from(stand.child.id()).expect("pid");
    drop(stand);
    let found = MacListeners.listeners().expect("the listing");
    assert_eq!(of(&found, pid), Vec::new());
}
