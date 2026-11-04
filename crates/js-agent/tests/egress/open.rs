//! The ceiling `open` and a `none` job under a ceiling, from inside real containers started by
//! the agent's `CrunRuntime`.

use std::net::SocketAddr;

use js_agent::egress::{Config, Egress, Flags};
use js_agent::lifecycle::{Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent_wire::LogTail;
use js_types::JobNetwork;

use crate::container::{PROBE, RuntimeHost, interfaces, section};
use crate::sys::{
    Far, Holder, Server, allow_list, far, links, members, packets, serial, tcp, wired,
};

/// The ceiling `open` with one listed endpoint and the default pool and programs.
fn open() -> Config {
    let allow = vec!["192.0.2.10:9300".to_owned()];
    Config::from_flags(&Flags {
        mode: Some("open"),
        allow: &allow,
        ..Flags::default()
    })
    .expect("a sound configuration")
}

/// Waits for the run `id` to end; its status and what it printed.
fn ended(host: &mut RuntimeHost, id: &RunHandle) -> (Status, LogTail) {
    host.runtime.wait_ended(id);
    let status = host.runtime.poll(id).expect("polled");
    let tail = host
        .runtime
        .log_tail(id)
        .expect("a tail")
        .unwrap_or_default();
    (status, tail)
}

/// The probe line of `target` in the section `reach` of `out`.
fn probed(out: &str, target: &str) -> String {
    section(out, "reach")
        .lines()
        .find(|line| line.starts_with(&format!("reach {target} ")))
        .unwrap_or_else(|| panic!("no probe of {target} in\n{out}"))
        .to_owned()
}

/// Asserts the ceiling `open` end to end. A neighbour (a holder wired as an `allow-list` run,
/// block 0, `198.18.0.2`, listening on port 7000) is up; an `open` job started through the
/// runtime (block 1) connects to the far host's address that is not on the list and gets its
/// answer, through the `open` rule (its counter moved) and masqueraded; its connections to the
/// metadata address, to the neighbour and to the host's own end of its link are refused, each
/// by its own rule, and none of those listeners saw a connection. While it runs its interface
/// is in `openifs` and not in `jobifs`; its removal leaves neither. Catches an open accept
/// placed above a refusal (the metadata address or the neighbour would answer), an open job
/// that reaches the host, and an open run registered as an allow-list one.
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn open_job_reaches_an_address_outside_the_list_but_not_metadata_or_a_neighbour() {
    let _serial = serial();
    let far_host = Far::new();
    let unlisted = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let metadata = Server::tcp(&far_host.ns, far::METADATA);
    let egress = Egress::start(open()).expect("start");
    egress.check().expect("the firewall reads back as loaded");
    let neighbour = Holder::new();
    let wired_neighbour = egress
        .attach("neighbour", neighbour.pid(), JobNetwork::AllowList)
        .expect("attach the neighbour");
    assert_eq!(wired_neighbour.interface(), Some("jsv0"));
    let on_neighbour = Server::tcp(&neighbour.ns, "0.0.0.0:7000".parse::<SocketAddr>().unwrap());
    assert!(tcp(&neighbour.ns, "127.0.0.1:7000".parse().unwrap()).connected());
    let local = on_neighbour.accepted();

    let mut host = RuntimeHost::new("open", &egress);
    host.runtime
        .check_egress()
        .expect("checked before the create");
    let script = format!(
        "{PROBE}echo '--reach'; p 192.0.2.40 9300; p 169.254.169.254 80; p 198.18.0.2 7000; \
         p 198.18.0.5 80; echo '--end'"
    );
    let spec = RunSpec {
        network: JobNetwork::Open,
        ..host.spec("open", &script)
    };
    let (open_rule, metadata_rule, inbound, input) = (
        packets("open"),
        packets("metadata"),
        packets("inbound"),
        packets("input"),
    );
    let id = host
        .runtime
        .create_and_start(&spec)
        .unwrap_or_else(|why| panic!("started: {why}"));
    assert_eq!(members("openifs"), ["jsv1"]);
    assert_eq!(wired(), ["jsv0"], "the open run is not in jobifs");
    let (status, tail) = ended(&mut host, &id);
    assert!(
        matches!(status, Status::Exited(Exit { code: 0, .. })),
        "{status:?}: {tail:?}"
    );
    let out = &tail.stdout;
    assert!(probed(out, "192.0.2.40:9300").contains("pong"), "{tail:?}");
    for refused in ["169.254.169.254:80", "198.18.0.2:7000", "198.18.0.5:80"] {
        let line = probed(out, refused);
        assert!(!line.contains("pong"), "{line}");
    }
    assert!(unlisted.accepted() >= 1);
    assert_eq!(
        unlisted.peers().first(),
        Some(&std::net::IpAddr::V4(far::HOST)),
        "masquerade"
    );
    assert_eq!(metadata.accepted(), 0, "the metadata address answered");
    assert_eq!(
        on_neighbour.accepted(),
        local,
        "an open job reached a neighbour"
    );
    assert!(packets("open") > open_rule, "the open rule let it through");
    assert!(
        packets("metadata") > metadata_rule,
        "the metadata range, by name"
    );
    assert!(
        packets("inbound") > inbound,
        "the neighbour, as traffic to a job"
    );
    assert!(packets("input") > input, "the host's own end of the link");
    egress.check().expect("still as loaded");
    host.runtime.remove(&id).expect("removed");
    assert!(members("openifs").is_empty(), "{:?}", members("openifs"));
    drop(wired_neighbour);
    assert!(links("jsv").is_empty(), "{:?}", links("jsv"));
}

/// Asserts that a job that asks for no network has loopback only under a ceiling that wires
/// others: through the runtime of an `allow-list` agent, its container's only interface is
/// `lo`, its routing table is empty, its `resolv.conf` is the backend's loopback one, it
/// started at once (no wait for a route), a connection to the allowed endpoint fails as
/// unreachable and the far host's listener sees nothing; no veth was made and no set holds a
/// member. Catches a `none` job wired because its host wires others, and one held for a route
/// nobody adds (it would not run within the route wait).
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn a_none_job_has_loopback_only() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let egress = Egress::start(allow_list(&["192.0.2.10:9300"], &["192.0.2.53"])).expect("start");
    let mut host = RuntimeHost::new("quiet", &egress);
    let script = format!(
        "{PROBE}echo '--dev'; cat /proc/net/dev; echo '--route'; cat /proc/net/route; \
         echo '--resolv'; cat /etc/resolv.conf; echo '--reach'; p 192.0.2.10 9300; \
         echo '--end'"
    );
    let spec = RunSpec {
        network: JobNetwork::None,
        ..host.spec("quiet", &script)
    };
    let id = host
        .runtime
        .create_and_start(&spec)
        .unwrap_or_else(|why| panic!("started: {why}"));
    assert!(
        links("jsv").is_empty(),
        "a none job got a veth: {:?}",
        links("jsv")
    );
    assert!(wired().is_empty() && members("openifs").is_empty());
    let (status, tail) = ended(&mut host, &id);
    assert!(
        matches!(status, Status::Exited(Exit { code: 0, .. })),
        "{status:?}: {tail:?}"
    );
    let out = &tail.stdout;
    assert_eq!(interfaces(&section(out, "dev")), ["lo"], "{tail:?}");
    let route = section(out, "route");
    assert_eq!(route.lines().count(), 1, "only the header: {route}");
    let resolv = section(out, "resolv");
    assert!(
        resolv.contains("nameserver 127.0.0.1") && !resolv.contains("192.0.2.53"),
        "{resolv}"
    );
    let line = probed(out, "192.0.2.10:9300").to_lowercase();
    assert!(
        !line.contains("pong") && line.contains("unreachable"),
        "{line}"
    );
    assert_eq!(allowed.accepted(), 0, "a packet left the container");
    host.runtime.remove(&id).expect("removed");
    assert!(links("jsv").is_empty() && wired().is_empty());
}
