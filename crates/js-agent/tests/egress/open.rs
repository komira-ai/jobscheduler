//! The ceiling `open` and a `none` job under a ceiling, from inside real containers started by
//! the agent's `CrunRuntime`.

use std::fs::File;
use std::net::SocketAddr;

use js_agent::egress::{Config, Egress};
use js_agent::lifecycle::{Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent_wire::LogTail;
use js_types::JobNetwork;

use crate::container::{PROBE, RuntimeHost, interfaces, section};
use crate::sys::{
    Far, Holder, Reach, Server, allow_list, far, links, members, packets, serial, tcp, udp, wired,
};

/// The ceiling `open` with one listed endpoint and the default pool and programs.
fn open() -> Config {
    crate::sys::open(&["192.0.2.10:9300"], &[])
}

/// The host's own network namespace, the test's.
fn host_ns() -> File {
    File::open("/proc/self/ns/net").expect("the host's namespace")
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
/// metadata address, to the neighbour and to a service listening on the host (reached at the
/// host's own end of the job's link, and answering a connection from the host itself) are
/// refused, each by its own rule, and none of those listeners saw a connection. While it runs its interface
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
    let on_host = Server::tcp(&host_ns(), "0.0.0.0:7001".parse::<SocketAddr>().unwrap());
    assert!(
        tcp(&host_ns(), "127.0.0.1:7001".parse().unwrap()).connected(),
        "the host's listener answers the host"
    );
    let host_local = on_host.accepted();

    let mut host = RuntimeHost::new("open", &egress);
    host.runtime
        .check_egress()
        .expect("checked before the create");
    let script = format!(
        "{PROBE}echo '--reach'; p 192.0.2.40 9300; p 169.254.169.254 80; p 198.18.0.2 7000; \
         p 198.18.0.5 7001; echo '--end'"
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
    for refused in ["169.254.169.254:80", "198.18.0.2:7000", "198.18.0.5:7001"] {
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
    assert_eq!(
        on_host.accepted(),
        host_local,
        "an open job reached the host's service"
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

/// Asserts that an `allow-list` job under an `open` ceiling keeps its list while an `open` job
/// beside it reaches any address, with endpoints configured and with resolvers only. With the
/// endpoint `192.0.2.10:9300` the allow-list holder reaches it and not the far host's address
/// that is not listed, which the open holder reaches at the same time. With no endpoint and the
/// resolver `192.0.2.53`, the allow-list holder's query to the resolver is answered while TCP
/// to the far host (listed before, now not) is refused, and the open holder reaches both.
/// Catches an open ceiling that widens its allow-list jobs, and one whose resolvers are not
/// reachable for them.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn an_allow_list_job_under_an_open_ceiling_keeps_its_list() {
    let _serial = serial();
    let far_host = Far::new();
    let listed = Server::tcp(&far_host.ns, far::ALLOWED);
    let unlisted = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let resolver = Server::udp(&far_host.ns, far::DNS);
    for (allow, dns) in [
        (&["192.0.2.10:9300"][..], &[][..]),
        (&[][..], &["192.0.2.53"][..]),
    ] {
        let egress = Egress::start(crate::sys::open(allow, dns)).expect("start");
        let (narrow, wide) = (Holder::new(), Holder::new());
        let _narrow = egress
            .attach("narrow", narrow.pid(), JobNetwork::AllowList)
            .expect("attach the allow-list run");
        let _wide = egress
            .attach("wide", wide.pid(), JobNetwork::Open)
            .expect("attach the open run");
        assert_eq!(wired(), ["jsv0"]);
        assert_eq!(members("openifs"), ["jsv1"]);
        let (before_listed, before_unlisted) = (listed.accepted(), unlisted.accepted());
        let reach_listed = tcp(&narrow.ns, far::ALLOWED);
        let reach_unlisted = tcp(&narrow.ns, far::OTHER_HOST);
        assert!(
            matches!(reach_unlisted, Reach::Failed(_)),
            "{reach_unlisted:?}"
        );
        if allow.is_empty() {
            assert!(matches!(reach_listed, Reach::Failed(_)), "{reach_listed:?}");
            assert_eq!(listed.accepted(), before_listed, "no endpoint is listed");
            let asked = resolver.accepted();
            assert!(udp(&narrow.ns, far::DNS), "the resolver did not answer");
            assert_eq!(resolver.accepted(), asked + 1);
        } else {
            assert!(reach_listed.connected(), "{reach_listed:?}");
            assert_eq!(listed.accepted(), before_listed + 1);
        }
        assert_eq!(
            unlisted.accepted(),
            before_unlisted,
            "the allow-list run reached an address outside its list"
        );
        assert!(tcp(&wide.ns, far::OTHER_HOST).connected(), "the open run");
        assert_eq!(unlisted.accepted(), before_unlisted + 1);
        if !allow.is_empty() {
            continue;
        }
        let asked = resolver.accepted();
        assert!(udp(&wide.ns, far::DNS), "the open run asked the resolver");
        assert_eq!(resolver.accepted(), asked + 1);
    }
}
