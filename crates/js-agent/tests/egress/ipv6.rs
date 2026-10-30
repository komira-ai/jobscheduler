//! IPv6 is off for a job, with a before/after control.

use js_agent::egress::Egress;

use crate::sys::{
    Far, Holder, Reach, Server, allow_list, in_netns, ip, ip_in, packets, serial, sysctl,
    sysctl_get, tcp, text,
};

fn v6() -> std::net::SocketAddr {
    crate::sys::far::V6.parse().expect("an IPv6 endpoint")
}

/// Removes the control's veth pair when dropped.
struct Raw;

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crate::sys::run(&crate::sys::tool("ip"), &["link", "del", "rawh0"]);
    }
}

/// Asserts that IPv6 is off, and that the test that says so can fail. Before: a namespace
/// wired by hand, as the agent would have left it with no IPv6 handling (a veth pair, an
/// IPv6 address on each end, a default route, forwarding on), reaches an IPv6 listener on the
/// far host, so the path, the listener and the probe are real. After: a namespace wired by
/// the agent has no IPv6 address (the host end and the job end both have `disable_ipv6`),
/// and a job that switches IPv6 on in its own namespace, gives itself an address and a
/// route reaches nothing: the far listener counts no connection. The host's switch is then
/// turned off too, and a static neighbour entry stands in for the discovery the host would
/// not answer, so that the packet does arrive at the host: the firewall's own IPv6 rule
/// drops it (its counter moved) and the listener still counts nothing, so the rule is a
/// second layer and not a restatement of the first.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn ipv6_is_off_for_a_wired_namespace_and_on_for_one_wired_by_hand() {
    let _serial = serial();
    let far_host = Far::new();
    far_host.ip(&["-6", "route", "add", "default", "via", "fd00:f::1"]);
    let listener = Server::tcp(&far_host.ns, v6());

    // Before.
    {
        let holder = Holder::new();
        let _raw = Raw;
        ip(&[
            "link", "add", "rawh0", "type", "veth", "peer", "name", "rawp0",
        ]);
        ip(&[
            "link",
            "set",
            "rawp0",
            "netns",
            &holder.pid().to_string(),
            "name",
            "eth0",
        ]);
        ip(&["-6", "addr", "add", "fd00:a::1/64", "dev", "rawh0", "nodad"]);
        ip(&["link", "set", "rawh0", "up"]);
        for args in [
            vec!["link", "set", "lo", "up"],
            vec!["-6", "addr", "add", "fd00:a::2/64", "dev", "eth0", "nodad"],
            vec!["link", "set", "eth0", "up"],
            vec!["-6", "route", "add", "default", "via", "fd00:a::1"],
        ] {
            let output = ip_in(&holder.ns, &args);
            assert!(output.status.success(), "{args:?}: {}", text(&output));
        }
        let reached = tcp(&holder.ns, v6());
        assert!(
            reached.connected(),
            "the control cannot reach the IPv6 listener: {reached:?}"
        );
        assert_eq!(listener.accepted(), 1);
    }

    // After.
    let egress = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("start");
    let holder = Holder::new();
    let attachment = egress.attach("v6", holder.pid()).expect("attach");
    let addresses = text(&ip_in(&holder.ns, &["-6", "addr", "show"]));
    assert!(
        !addresses.contains("inet6"),
        "the job end has an IPv6 address: {addresses}"
    );
    assert_eq!(
        sysctl_get("net/ipv6/conf/jsv0/disable_ipv6"),
        "1",
        "the host end"
    );
    let inside = in_netns(&holder.ns, || {
        std::fs::read_to_string("/proc/sys/net/ipv6/conf/eth0/disable_ipv6").unwrap_or_default()
    });
    assert_eq!(inside.trim(), "1", "the job end");

    // The job turns IPv6 on in its own namespace and addresses itself.
    in_netns(&holder.ns, || {
        for scope in ["all", "default", "eth0"] {
            std::fs::write(format!("/proc/sys/net/ipv6/conf/{scope}/disable_ipv6"), "0")
                .expect("a capable job can switch IPv6 on in its own namespace");
        }
    });
    for args in [
        vec!["-6", "addr", "add", "fd00:b::2/64", "dev", "eth0", "nodad"],
        vec!["link", "set", "eth0", "up"],
        vec!["-6", "route", "add", "default", "via", "fd00:b::1"],
    ] {
        let output = ip_in(&holder.ns, &args);
        assert!(output.status.success(), "{args:?}: {}", text(&output));
    }
    let reached = tcp(&holder.ns, v6());
    assert!(matches!(reached, Reach::Failed(_)), "{reached:?}");
    assert_eq!(
        listener.accepted(),
        1,
        "an IPv6 connection of the wired job arrived"
    );

    // The host's switch off as well: only the firewall is left.
    sysctl("net/ipv6/conf/jsv0/disable_ipv6", "0");
    ip(&["-6", "addr", "add", "fd00:b::1/64", "dev", "jsv0", "nodad"]);
    let mac = std::fs::read_to_string("/sys/class/net/jsv0/address").expect("mac");
    let output = ip_in(
        &holder.ns,
        &[
            "-6",
            "neigh",
            "replace",
            "fd00:b::1",
            "lladdr",
            mac.trim(),
            "dev",
            "eth0",
            "nud",
            "permanent",
        ],
    );
    assert!(output.status.success(), "{}", text(&output));
    let before = packets("ipv6");
    let reached = tcp(&holder.ns, v6());
    assert!(matches!(reached, Reach::Failed(_)), "{reached:?}");
    assert!(
        packets("ipv6") > before,
        "the firewall's IPv6 rule did not see the packet"
    );
    assert_eq!(listener.accepted(), 1);
    drop(attachment);
}
