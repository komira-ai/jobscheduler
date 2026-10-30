//! An allow-list from inside a real container, and through a second host's routing.

use js_agent::egress::Egress;
use js_contain::container::Ended;

use crate::container::{ContainerHost, Network, PROBE, block_on, interfaces, section};
use crate::sys::{
    Far, Holder, Reach, Server, allow_list, far, masquerade_packets, nft, packets, serial, tcp,
};

fn reached(out: &str, target: &str) -> String {
    section(out, "reach")
        .lines()
        .find(|line| line.starts_with(&format!("reach {target} ")))
        .unwrap_or_else(|| panic!("no probe of {target} in\n{out}"))
        .to_owned()
}

/// Asserts the allow-list end to end. A real container, made by `crun` as the job user with
/// a user and network namespace of its own, is wired by the agent from the host while its
/// init waits, then runs a job that connects to five places. Only the allowed endpoint
/// answers (the job printed its `pong`, the far host's listener counted it); another port of
/// the same host, another host on the same port, the metadata address and the host's own end
/// of the job's link are refused, each by its own rule (the rule's packet counter moved), and
/// none of those listeners saw a connection. The path is forwarded (the far host is another
/// network namespace behind the host's routing, so the counted rule is the forward chain's,
/// never local delivery) and masqueraded (the far host saw the host's address, not the
/// job's). The job's view is one `eth0` and a default route, its `resolv.conf` names the
/// configured resolver only, and the firewall still reads back as loaded. The job connected
/// under the seccomp profile resolved for the capabilities a job gets (none).
#[test]
#[ignore = "needs root, crun, busybox, a job user: see the module docs"]
fn an_allowed_endpoint_answers_and_everything_else_is_refused_by_its_rule() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let other_port = Server::tcp(&far_host.ns, far::OTHER_PORT);
    let other_host = Server::tcp(&far_host.ns, far::OTHER_HOST);
    let metadata = Server::tcp(&far_host.ns, far::METADATA);
    let config = allow_list(&["192.0.2.10:9300"], &["192.0.2.53"]);
    let egress = Egress::start(config.clone()).expect("start");
    egress.check().expect("the firewall reads back as loaded");
    let script = format!(
        "{PROBE}echo started; echo '--dev'; cat /proc/net/dev; echo '--route'; cat /proc/net/route; \
         echo '--resolv'; cat /etc/resolv.conf; echo '--reach'; \
         p 192.0.2.10 9300; p 192.0.2.10 9301; p 192.0.2.40 9300; p 169.254.169.254 80; \
         p 198.18.0.1 80; echo '--end'"
    );
    let outcome = block_on(async {
        let network = Network {
            resolv_conf: config.resolv_conf(),
            route_wait: config.route_wait(),
        };
        let host = ContainerHost::new("allow", network).await;
        egress.check().expect("checked before the create");
        let live = host.start(&script).await;
        let attachment = egress.attach("allow", live.pid).expect("attach");
        assert_eq!(attachment.interface(), Some("jsv0"));
        let outcome = live.finish().await;
        attachment.release().expect("release");
        outcome
    });
    assert_eq!(outcome.ended, Ok(Ended::Exited(0)), "{}", outcome.said);
    let out = &outcome.stdout;
    assert!(
        out.lines().next() == Some("started"),
        "the job did not start: {}",
        outcome.said
    );
    let mut names = interfaces(&section(out, "dev"));
    names.sort();
    assert_eq!(names, ["eth0", "lo"], "{}", outcome.said);
    assert!(
        section(out, "route").contains("010012C6"),
        "a default route via 198.18.0.1: {}",
        outcome.said
    );
    let resolv = section(out, "resolv");
    assert!(
        resolv.contains("nameserver 192.0.2.53") && !resolv.contains("127.0.0.1"),
        "{resolv}"
    );
    assert!(
        reached(out, "192.0.2.10:9300").contains("pong"),
        "{}",
        outcome.said
    );
    for refused in [
        "192.0.2.10:9301",
        "192.0.2.40:9300",
        "169.254.169.254:80",
        "198.18.0.1:80",
    ] {
        let line = reached(out, refused);
        assert!(!line.contains("pong"), "{line}");
    }
    assert!(allowed.accepted() >= 1);
    assert_eq!(
        allowed.peers().first(),
        Some(&std::net::IpAddr::V4(far::HOST)),
        "masquerade"
    );
    assert_eq!(
        other_port.accepted() + other_host.accepted() + metadata.accepted(),
        0,
        "a refused connection reached a listener"
    );
    assert!(
        packets("allow-0") >= 1,
        "the allowance is what let it through"
    );
    assert!(
        packets("default-reject") >= 2,
        "the other port and the other host"
    );
    assert!(packets("metadata") >= 1, "the metadata range, by name");
    assert!(packets("input") >= 1, "the host's own end of the link");
    assert!(masquerade_packets() >= 1);
    egress.check().expect("still as loaded");
}

/// Asserts that the path to another host needs both halves of what the host does: forwarding
/// and the masquerade. The far host is another network namespace that has no route to the
/// jobs' addresses. With the masquerade rule's chain emptied the job's connection to the
/// allowed endpoint does not complete (the listener never accepts: its answer has nowhere to
/// go), and with the one rule put back it completes and the far host sees the host's
/// address. Without this control the success above could be a path that never needed the
/// host to act as a router.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn the_far_host_answers_only_through_the_masquerade() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let egress = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("start");
    let holder = Holder::new();
    let attachment = egress.attach("masq", holder.pid()).expect("attach");

    nft(&["flush", "chain", "ip", "js_crun_egress_nat", "postrouting"]);
    let without = tcp(&holder.ns, far::ALLOWED);
    assert!(matches!(without, Reach::Failed(_)), "{without:?}");
    assert_eq!(
        allowed.accepted(),
        0,
        "the far host accepted a connection it cannot answer"
    );

    nft(&[
        "add",
        "rule",
        "ip",
        "js_crun_egress_nat",
        "postrouting",
        "ip",
        "saddr",
        "198.18.0.0/16",
        "ip",
        "daddr",
        "!=",
        "198.18.0.0/16",
        "counter",
        "masquerade",
        "comment",
        "\"masquerade\"",
    ]);
    let with = tcp(&holder.ns, far::ALLOWED);
    assert!(with.connected(), "{with:?}");
    assert_eq!(allowed.peers(), [std::net::IpAddr::V4(far::HOST)]);
    assert!(
        packets("allow-0") >= 2,
        "forwarded both times, so it was the masquerade that differed"
    );
    drop(attachment);
}
