//! The forwards of declared ports against the real kernel: a declared port reaches its job
//! through the host's address and nothing else reaches any job, one host port is held by one
//! job at a time, and the check reads the forwards back. The job is a holder with a listener
//! in its namespace; the client is in the far namespace, another host behind the host's
//! routing, so every path here is forwarded, never local delivery.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use js_agent::egress::{Config, Egress, EgressError, Flags};
use js_types::{JobNetwork, PortMap};
use serde_json::Value;

use crate::sys::{Far, Holder, Reach, Server, delete_rule, far, links, nft, packets, serial, tcp};

/// The allow-list of these tests, which lets jobs declare the host port 8080.
fn forwarding() -> Config {
    let allow = ["192.0.2.10:9300".to_owned()];
    let ports = ["8080".to_owned()];
    Config::from_flags(&Flags {
        mode: Some("allow-list"),
        allow: &allow,
        forward_ports: &ports,
        ..Flags::default()
    })
    .expect("a sound configuration")
}

/// `host -> container`, declared.
fn port(host: u16, container: u16) -> Vec<PortMap> {
    vec![PortMap { host, container }]
}

/// The host's address on the link to the far host, port `port`.
fn host(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(far::HOST, port))
}

/// The address of the job of block `block` of the default pool, port `port`.
fn job(block: u8, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(198, 18, 0, block * 4 + 2),
        port,
    ))
}

/// Any address of a namespace, port `port`: what a job's listener binds.
fn any(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
}

/// The comments of the rules of both tables that start with `fwd-`.
fn forward_rules() -> Vec<String> {
    let mut found = Vec::new();
    for (family, table) in [("inet", "js_crun_egress"), ("ip", "js_crun_egress_nat")] {
        let listing: Value =
            serde_json::from_str(&nft(&["-j", "list", "table", family, table])).expect("json");
        found.extend(
            listing["nftables"]
                .as_array()
                .expect("nftables")
                .iter()
                .filter_map(|item| item["rule"]["comment"].as_str())
                .filter(|comment| comment.starts_with("fwd-"))
                .map(str::to_owned),
        );
    }
    found
}

/// Deletes the rule commented `comment` from the chain `prerouting` of the NAT table.
fn delete_nat_rule(comment: &str) {
    let listing: Value = serde_json::from_str(&nft(&[
        "-j",
        "list",
        "chain",
        "ip",
        "js_crun_egress_nat",
        "prerouting",
    ]))
    .expect("nft json");
    let handle = listing["nftables"]
        .as_array()
        .expect("nftables")
        .iter()
        .filter_map(|item| item.get("rule"))
        .find(|rule| rule["comment"] == comment)
        .and_then(|rule| rule["handle"].as_u64())
        .expect("the rule is there");
    nft(&[
        "delete",
        "rule",
        "ip",
        "js_crun_egress_nat",
        "prerouting",
        "handle",
        &handle.to_string(),
    ]);
}

/// Asserts the forward end to end. A job (a holder with a listener on port 80 in its
/// namespace) declares `8080 -> 80`; a client on the far host connects to the host's own
/// address on port 8080 and gets the job's answer. The job saw the client's own address (the
/// forward rewrites the destination only), the forward's acceptance counted the connection,
/// and the firewall reads back as loaded with the forward of the live job.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_declared_port_reaches_the_job_through_the_host_address() {
    let _serial = serial();
    let far_host = Far::new();
    let egress = Egress::start(forwarding()).expect("start");
    let holder = Holder::new();
    let listener = Server::tcp(&holder.ns, any(80));
    let attachment = egress
        .attach("fwd", holder.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("attach");
    egress.check().expect("the forward of a live job");
    let before = packets("fwd-8080");
    let reached = tcp(&far_host.ns, host(8080));
    assert!(reached.connected(), "{reached:?}");
    assert_eq!(listener.accepted(), 1);
    assert_eq!(
        listener.peers(),
        [IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2))],
        "the job sees the client"
    );
    assert!(
        packets("fwd-8080") > before,
        "the forward's acceptance let it through"
    );
    egress.check().expect("still as loaded");
    attachment.release().expect("release");
}

/// Asserts that the forward is one port to one destination and nothing wider. With the job
/// listening on ports 80 and 81 and declaring `8080 -> 80`, and the far host routing the
/// pool through the host: another port of the host (8081) is refused and reaches no
/// listener; the job's own address, on its declared container port and on another port, is
/// refused by the `inbound` rule (its counter moves, the forward's does not), since those
/// packets were not DNATed; and the declared port still answers.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn an_undeclared_port_is_refused() {
    let _serial = serial();
    let far_host = Far::new();
    far_host.ip(&["route", "add", "198.18.0.0/16", "via", "192.0.2.1"]);
    let egress = Egress::start(forwarding()).expect("start");
    let holder = Holder::new();
    let declared = Server::tcp(&holder.ns, any(80));
    let other = Server::tcp(&holder.ns, any(81));
    let attachment = egress
        .attach(
            "narrow",
            holder.pid(),
            JobNetwork::AllowList,
            &port(8080, 80),
        )
        .expect("attach");
    let (forwarded, inbound) = (packets("fwd-8080"), packets("inbound"));
    for to in [host(8081), job(0, 80), job(0, 81)] {
        let reached = tcp(&far_host.ns, to);
        assert!(matches!(reached, Reach::Failed(_)), "{to}: {reached:?}");
    }
    assert_eq!(declared.accepted() + other.accepted(), 0);
    assert_eq!(
        packets("fwd-8080"),
        forwarded,
        "the forward accepted nothing"
    );
    assert!(
        packets("inbound") >= inbound + 2,
        "the job's own address, both ports"
    );
    let reached = tcp(&far_host.ns, host(8080));
    assert!(reached.connected(), "{reached:?}");
    assert_eq!(declared.accepted(), 1);
    attachment.release().expect("release");
}

/// Asserts that a forward does not open another job. Two jobs run on the agent, the first
/// declaring `8080 -> 80` and the second nothing, each listening on 80 and 8080: the host's
/// port 8080 reaches the first job only; the second job's address, on either port, and the
/// host's port 8081 are refused, from the far host, and the first job cannot reach the second
/// either. Neither job reaches the forward through the host's addresses (its gateway, or the
/// host's address on the far link): a job's packet to the host is not DNATed and meets the
/// `input` refusal, whose counter moves while the forward's does not.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_job_with_no_ports_has_no_inbound_path() {
    let _serial = serial();
    let far_host = Far::new();
    far_host.ip(&["route", "add", "198.18.0.0/16", "via", "192.0.2.1"]);
    let egress = Egress::start(forwarding()).expect("start");
    let (open, closed) = (Holder::new(), Holder::new());
    let servers: Vec<Server> = [&open, &closed]
        .iter()
        .flat_map(|holder| {
            [
                Server::tcp(&holder.ns, any(80)),
                Server::tcp(&holder.ns, any(8080)),
            ]
        })
        .collect();
    let first = egress
        .attach("open", open.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("attach");
    let second = egress
        .attach("closed", closed.pid(), JobNetwork::AllowList, &[])
        .expect("attach");
    assert_eq!(second.interface(), Some("jsv1"));
    for to in [job(1, 80), job(1, 8080), host(8081)] {
        let reached = tcp(&far_host.ns, to);
        assert!(matches!(reached, Reach::Failed(_)), "{to}: {reached:?}");
    }
    for to in [job(1, 80), job(1, 8080)] {
        let reached = tcp(&open.ns, to);
        assert!(
            matches!(reached, Reach::Failed(_)),
            "from the first job, {to}: {reached:?}"
        );
    }
    let (forwarded, input) = (packets("fwd-8080"), packets("input"));
    for (from, gateway) in [(&open, 1), (&closed, 5)] {
        let gateway = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(198, 18, 0, gateway), 8080));
        for to in [gateway, host(8080)] {
            let reached = tcp(&from.ns, to);
            assert!(
                matches!(reached, Reach::Failed(_)),
                "through the host, {to}: {reached:?}"
            );
        }
    }
    assert_eq!(
        packets("fwd-8080"),
        forwarded,
        "a job's packet was forwarded"
    );
    assert!(packets("input") >= input + 4, "each refused by input");
    let reached = tcp(&far_host.ns, host(8080));
    assert!(reached.connected(), "{reached:?}");
    let accepted: Vec<u64> = servers.iter().map(Server::accepted).collect();
    assert_eq!(accepted, [1, 0, 0, 0], "only the first job's port 80");
    drop((first, second));
}

/// Asserts that one host port is one job's. A second job that declares the host port the
/// first one holds is refused `PortTaken`, and nothing of it is left (only the first job's
/// interface exists); the port still reaches the first job. Once the first job is released,
/// the second job declares the port again and the port reaches it.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn two_jobs_cannot_hold_the_same_host_port() {
    let _serial = serial();
    let far_host = Far::new();
    let egress = Egress::start(forwarding()).expect("start");
    let (a, b) = (Holder::new(), Holder::new());
    let (on_a, on_b) = (Server::tcp(&a.ns, any(80)), Server::tcp(&b.ns, any(80)));
    let first = egress
        .attach("a", a.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("attach");
    let refused = egress
        .attach("b", b.pid(), JobNetwork::AllowList, &port(8080, 80))
        .map(drop);
    assert_eq!(refused, Err(EgressError::PortTaken { port: 8080 }));
    assert_eq!(links("jsv"), ["jsv0"], "nothing of the second job is left");
    assert!(tcp(&far_host.ns, host(8080)).connected());
    assert_eq!((on_a.accepted(), on_b.accepted()), (1, 0));
    first.release().expect("release");
    let second = egress
        .attach("b", b.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("the port is free");
    assert!(tcp(&far_host.ns, host(8080)).connected());
    assert_eq!((on_a.accepted(), on_b.accepted()), (1, 1));
    second.release().expect("release");
}

/// Asserts that the check reads the forwards back and the release leaves none. With a job's
/// forward in place the check passes; with its acceptance deleted it is `Rules`; the release
/// still removes what is left. With the forward made again and its DNAT rule replaced by one
/// to another port under the same comment, the check is `Rules` again. After the release no
/// rule under `fwd-` is left in either table, no interface of the agent's is left, and the
/// check passes.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_changed_or_deleted_forward_is_seen_by_the_check_and_a_released_one_is_gone() {
    let _serial = serial();
    let _far = Far::new();
    let egress = Egress::start(forwarding()).expect("start");
    let holder = Holder::new();
    let attachment = egress
        .attach("seen", holder.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("attach");
    egress.check().expect("as loaded");
    assert_eq!(forward_rules(), ["fwd-8080", "fwd-8080"]);
    delete_rule("fwd-8080");
    assert_eq!(egress.check(), Err(EgressError::Rules));
    attachment.release().expect("release");
    assert_eq!(forward_rules(), Vec::<String>::new());
    egress.check().expect("nothing of the run is left");

    let attachment = egress
        .attach("seen", holder.pid(), JobNetwork::AllowList, &port(8080, 80))
        .expect("attach");
    delete_nat_rule("fwd-8080");
    nft(&[
        "add",
        "rule",
        "ip",
        "js_crun_egress_nat",
        "prerouting",
        "iifname",
        "!=",
        "\"jsv*\"",
        "fib",
        "daddr",
        "type",
        "local",
        "tcp",
        "dport",
        "8080",
        "counter",
        "dnat",
        "to",
        "198.18.0.2:81",
        "comment",
        "\"fwd-8080\"",
    ]);
    assert_eq!(egress.check(), Err(EgressError::Rules));
    attachment.release().expect("release");
    assert_eq!(forward_rules(), Vec::<String>::new());
    assert_eq!(links("jsv"), Vec::<String>::new());
    egress.check().expect("back as loaded");
}
