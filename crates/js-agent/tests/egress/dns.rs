//! Name resolution only through the configured resolvers.

use js_agent::egress::Egress;

use crate::sys::{Far, Holder, Reach, Server, allow_list, far, packets, serial, tcp, udp};

/// Asserts that a wired namespace can ask the configured resolver and no other: a datagram
/// to port 53 of the configured address is answered (the rule for it counted), one to another
/// address of the same host is refused by the default rule and the other resolver counts
/// nothing, and a configuration with no resolver gives no resolver at all (the same query to
/// the same address gets no answer). The `resolv.conf` text the configuration implies names
/// the configured resolver only; the end-to-end test reads it from inside a container.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn only_the_configured_resolver_is_reachable() {
    let _serial = serial();
    let far_host = Far::new();
    let configured = Server::udp(&far_host.ns, far::DNS);
    let other = Server::udp(&far_host.ns, far::DNS_OTHER);

    let config = allow_list(&["192.0.2.10:9300"], &["192.0.2.53"]);
    assert_eq!(
        config.resolv_conf().as_deref(),
        Some("nameserver 192.0.2.53\noptions timeout:1 attempts:1\n")
    );
    let egress = Egress::start(config).expect("start");
    let holder = Holder::new();
    let attachment = egress.attach("dns", holder.pid()).expect("attach");
    assert!(
        udp(&holder.ns, far::DNS),
        "the configured resolver did not answer"
    );
    assert_eq!(configured.accepted(), 1);
    assert!(packets("dns-udp-0") >= 1);
    let refused = packets("default-reject");
    assert!(
        !udp(&holder.ns, far::DNS_OTHER),
        "another resolver answered"
    );
    assert_eq!(
        other.accepted(),
        0,
        "a datagram reached a resolver nobody configured"
    );
    assert!(
        packets("default-reject") > refused,
        "its own rule refused it"
    );
    // TCP port 53 too.
    let configured_tcp = Server::tcp(&far_host.ns, far::DNS);
    let other_tcp = Server::tcp(&far_host.ns, far::DNS_OTHER);
    assert!(
        tcp(&holder.ns, far::DNS).connected(),
        "TCP to the configured resolver"
    );
    assert!(packets("dns-tcp-0") >= 1);
    assert!(matches!(tcp(&holder.ns, far::DNS_OTHER), Reach::Failed(_)));
    assert_eq!(other_tcp.accepted(), 0);
    assert_eq!(configured_tcp.accepted(), 1);
    drop(attachment);

    let none = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("restart");
    assert_eq!(none.config().resolv_conf(), None);
    let holder = Holder::new();
    let attachment = none.attach("no-dns", holder.pid()).expect("attach");
    assert!(
        !udp(&holder.ns, far::DNS),
        "a resolver answered with none configured"
    );
    assert_eq!(configured.accepted(), 1);
    assert!(matches!(tcp(&holder.ns, far::DNS), Reach::Failed(_)));
    assert_eq!(configured_tcp.accepted(), 1);
    drop(attachment);
}
