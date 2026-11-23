//! The decisions of the listener check over a fake table.

use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::sync::Mutex;

use super::{Listener, ListenerTable, denied_ports, describe, host_listeners, on_loopback};
use crate::mac_accounts::Slots;

/// The slots of the pool in these tests: uids 700 to 709.
fn slots() -> Slots {
    Slots::new(700, 10).expect("slots")
}

fn listener(addr: &str, port: u16, uid: u32, ruid: u32) -> Listener {
    Listener {
        addr: addr.parse().expect("address"),
        port,
        pid: 100 + i32::from(port % 100),
        uid,
        ruid,
        name: "svc".to_owned(),
    }
}

/// A table that answers what it was given, or an error once it was told to fail.
struct Fake(Mutex<io::Result<Vec<Listener>>>);

impl ListenerTable for Fake {
    fn listeners(&self) -> io::Result<Vec<Listener>> {
        match &*self.0.lock().expect("lock") {
            Ok(found) => Ok(found.clone()),
            Err(why) => Err(io::Error::new(why.kind(), why.to_string())),
        }
    }
}

/// Asserts which bound addresses a loopback connect reaches: loopback and the wildcard, in
/// IPv4, IPv6 and IPv4-mapped spellings, and no other address. Catches a check that misses a
/// wildcard listener (reachable as `127.0.0.1:PORT`), one that misses the mapped spelling of
/// a dual-stack socket, and one that would deny a service bound to an outside address only.
#[test]
fn a_loopback_connect_reaches_loopback_and_wildcard_listeners_only() {
    let reached = [
        "127.0.0.1",
        "127.1.2.3",
        "0.0.0.0",
        "::1",
        "::",
        "::ffff:127.0.0.1",
        "::ffff:0.0.0.0",
    ];
    let elsewhere = [
        "192.168.1.2",
        "10.77.0.21",
        "fe80::1",
        "2001:db8::1",
        "::ffff:10.0.0.1",
    ];
    for addr in reached {
        assert!(on_loopback(addr.parse::<IpAddr>().unwrap()), "{addr}");
    }
    for addr in elsewhere {
        assert!(!on_loopback(addr.parse::<IpAddr>().unwrap()), "{addr}");
    }
}

/// Asserts that a listener is the host's unless its effective or its real uid is a slot's,
/// with the slots' first and last uids and the uids just outside them, and that a listener
/// off loopback is never the host's to deny. Catches a job's own listener denied to the next
/// job (a test fixture of a build that waits on it), a set-uid process of a job counted as
/// the host's, and an off-by-one at either end of the range.
#[test]
fn a_listener_is_the_hosts_unless_a_slot_holds_it() {
    let found = vec![
        listener("127.0.0.1", 12049, 0, 0),
        listener("127.0.0.1", 9300, 260, 260),
        listener("127.0.0.1", 2001, 700, 700),
        listener("127.0.0.1", 2002, 709, 709),
        listener("127.0.0.1", 2003, 0, 705),
        listener("127.0.0.1", 2004, 705, 0),
        listener("::", 2005, 699, 699),
        listener("::1", 2006, 710, 710),
        listener("10.77.0.21", 2007, 0, 0),
    ];
    let ports: Vec<u16> = host_listeners(found, slots())
        .iter()
        .map(|host| host.port)
        .collect();
    assert_eq!(ports, [12049, 9300, 2005, 2006]);
}

/// Asserts that the denied ports are the configured ones and the found host listeners' ports,
/// each once and lowest first, a configured port denied with nothing listening on it and a
/// port both configured and found named once. Catches a configured port dropped while its
/// service is down, and a found listener's port left out because no flag names it.
#[test]
fn the_denied_ports_are_the_configured_and_the_found_ones() {
    let table = Fake(Mutex::new(Ok(vec![
        listener("127.0.0.1", 8021, 0, 0),
        listener("::1", 8021, 0, 0),
        listener("127.0.0.1", 9300, 260, 260),
        listener("0.0.0.0", 5000, 501, 501),
        listener("127.0.0.1", 3000, 703, 703),
    ])));
    let configured: BTreeSet<u16> = [12049, 9300].into();
    assert_eq!(
        denied_ports(&table, &configured, slots()).unwrap(),
        [5000, 8021, 9300, 12049]
    );
    let nothing = Fake(Mutex::new(Ok(Vec::new())));
    assert_eq!(
        denied_ports(&nothing, &configured, slots()).unwrap(),
        [9300, 12049]
    );
}

/// Asserts that a table that cannot be read fails the denied ports with its own error, and
/// gives no list. Catches a failed listing read as "no listener", which would start a run
/// with every unconfigured host service reachable.
#[test]
fn an_unreadable_table_gives_no_ports() {
    let table = Fake(Mutex::new(Err(io::Error::other(
        "process 88: Operation not permitted",
    ))));
    let why = denied_ports(&table, &BTreeSet::new(), slots()).unwrap_err();
    assert_eq!(why.to_string(), "process 88: Operation not permitted");
}

/// Asserts the log text of a host listener, IPv4 and IPv6. Catches an IPv6 address written
/// without brackets, which reads as part of the port.
#[test]
fn a_host_listener_is_described_by_process_uid_and_address() {
    assert_eq!(
        describe(&listener("127.0.0.1", 8021, 0, 0)),
        "svc (pid 121, uid 0) on 127.0.0.1:8021"
    );
    assert_eq!(
        describe(&listener("::1", 8021, 0, 0)),
        "svc (pid 121, uid 0) on [::1]:8021"
    );
}
