//! The host's own ports in a native run's profile: the ports of `--native-host-port` and of the
//! host's listeners that the runtime's table lists when the run starts, each denied; a job
//! account's listener and one off loopback not denied; a table that cannot be read failing the
//! start, and the agent's start when a job reaches loopback. Over the fake launcher, which keeps
//! the profile each run was given.

use std::io;
use std::sync::Arc;

use js_agent::lifecycle::Runtime;
use js_agent::mac_listen::{Listener, ListenerTable};
use js_agent::native::Ceiling;

use super::{Listening, Rig, me, native};

fn listener(addr: &str, port: u16, uid: u32) -> Listener {
    Listener {
        addr: addr.parse().expect("address"),
        port,
        pid: 300,
        uid,
        ruid: uid,
        name: "svc".to_owned(),
    }
}

/// A host whose listeners cannot be read.
struct Unreadable;

impl ListenerTable for Unreadable {
    fn listeners(&self) -> io::Result<Vec<Listener>> {
        Err(io::Error::other("process 88: Operation not permitted"))
    }
}

fn denies(port: u16) -> [String; 2] {
    [
        format!("(deny network* (local ip \"*:{port}\"))"),
        format!("(deny network* (remote ip \"localhost:{port}\"))"),
    ]
}

/// A rig at `ceiling`, its host ports `12049`, its host's listeners `found`.
fn rig_at(name: &str, ceiling: Ceiling, table: Arc<dyn ListenerTable>) -> Rig {
    let mut rig = Rig::with(name, |config| {
        config.network = ceiling;
        config.host_ports = [12049].into();
    });
    rig.rt = rig.rt.with_listener_table(table);
    rig
}

/// Asserts that a job of the level `none` under a `loopback` ceiling gets a profile that
/// denies the configured port with nothing listening on it, a root listener on 127.0.0.1 and
/// ::1, and another user's wildcard listener, and denies neither the listener of the run's own
/// slot account nor one bound to an outside address. Catches the runtime building the profile
/// without the listing, the configured port dropped while its service is down, and a job's
/// own listener denied to it.
#[test]
fn a_runs_profile_denies_the_host_ports_and_the_hosts_listeners() {
    let found = vec![
        listener("127.0.0.1", 8021, 0),
        listener("::1", 8021, 0),
        listener("0.0.0.0", 5000, me() + 1000),
        listener("127.0.0.1", 3000, me()),
        listener("10.0.0.1", 2222, 0),
    ];
    let mut rig = rig_at(
        "ports-denied",
        Ceiling::Loopback,
        Arc::new(Listening(found)),
    );
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap();
    let profile = rig.launcher.last().profile;
    assert!(profile.contains("(allow network* (remote ip \"localhost:*\"))"));
    for port in [5000, 8021, 12049] {
        for rule in denies(port) {
            assert_eq!(profile.matches(&rule).count(), 1, "{rule}:\n{profile}");
        }
    }
    for port in [3000, 2222] {
        assert!(
            !profile.contains(&format!(":{port}\"")),
            "{port}:\n{profile}"
        );
    }
}

/// Asserts that a host whose listeners cannot be read fails the start with the reason and
/// launches nothing, under a ceiling that gives the job loopback, and that under the ceiling
/// `off` (no network, no port to deny) the same host starts the job. Catches an unreadable
/// listing taken as "no listener", which would start a run with the host's services reachable.
#[test]
fn an_unreadable_host_fails_a_loopback_start_and_not_an_off_one() {
    let mut rig = rig_at("ports-unreadable", Ceiling::Loopback, Arc::new(Unreadable));
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let why = rig.rt.create_and_start(&spec).unwrap_err().to_string();
    assert!(
        why.ends_with("the host's listeners: process 88: Operation not permitted"),
        "{why}"
    );
    assert!(rig.launcher.launched().is_empty(), "nothing was started");

    let mut off = rig_at("ports-off", Ceiling::Off, Arc::new(Unreadable));
    let spec = off.spec("r1", native("/usr/bin/true", &[]));
    off.rt.create_and_start(&spec).unwrap();
    assert!(off.launcher.last().profile.contains("(deny network*)"));
}

/// Asserts that at the agent's start (`check_egress`) a ceiling above `off` reads the host's
/// listeners and fails, naming `--native-network` and the reason, when they cannot be read,
/// and passes when they can; and that the ceiling `off` does not read them. Catches an agent
/// that registers for jobs it would then fail one by one, and one that refuses to start over
/// a listing no job of it needs.
#[test]
fn the_agents_start_needs_the_listeners_only_when_a_job_reaches_loopback() {
    for ceiling in [Ceiling::Loopback, Ceiling::Open] {
        let rig = rig_at("ports-start", ceiling, Arc::new(Unreadable));
        let why = rig.rt.check_egress().unwrap_err().to_string();
        assert_eq!(
            why,
            "--native-network: a job reaches loopback, and the host's listeners cannot be read: \
             process 88: Operation not permitted",
            "{ceiling}"
        );
        let found = Arc::new(Listening(vec![listener("127.0.0.1", 8021, 0)]));
        let readable = rig_at("ports-start-ok", ceiling, found);
        assert!(readable.rt.check_egress().is_ok(), "{ceiling}");
    }
    let off = rig_at("ports-start-off", Ceiling::Off, Arc::new(Unreadable));
    assert!(off.rt.check_egress().is_ok());
}

/// Asserts that an agent whose host ports and host listeners together name more ports than a
/// profile holds (256) does not start, naming `--native-host-port` and the count, and that one
/// at the bound starts. Catches an agent that starts and then fails every run at its profile.
#[test]
fn the_agent_does_not_start_with_more_ports_to_deny_than_a_profile_holds() {
    let many = |count: u16| -> Arc<dyn ListenerTable> {
        let found = (20_000..20_000 + count)
            .map(|port| listener("127.0.0.1", port, 0))
            .collect();
        Arc::new(Listening(found))
    };
    let over = rig_at("ports-many", Ceiling::Loopback, many(256));
    let why = over.rt.check_egress().unwrap_err().to_string();
    assert_eq!(
        why,
        "--native-host-port: 257 ports to deny (these and the host's listeners), more than \
         the 256 a profile holds"
    );
    let at = rig_at("ports-at-bound", Ceiling::Loopback, many(255));
    assert!(at.rt.check_egress().is_ok());
}
