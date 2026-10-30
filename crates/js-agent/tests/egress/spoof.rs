//! A job cannot send from an address that is not its own.

use std::net::IpAddr;

use js_agent::egress::Egress;

use crate::sys::{
    Far, Holder, Reach, Server, allow_list, delete_rule, far, ip, ip_in, packets, serial, sysctl,
    sysctl_get, tcp, text,
};

/// Puts the host's reverse-path filtering back when dropped.
struct RpFilter {
    all: String,
}

impl RpFilter {
    /// Switches the kernel's own reverse-path filtering off, so that the firewall's rule is
    /// the only thing between a readdressed job and the allowed endpoint.
    fn off() -> Self {
        let all = sysctl_get("net/ipv4/conf/all/rp_filter");
        sysctl("net/ipv4/conf/all/rp_filter", "0");
        Self { all }
    }
}

impl Drop for RpFilter {
    fn drop(&mut self) {
        sysctl("net/ipv4/conf/all/rp_filter", &self.all);
    }
}

/// Gives the job in `holder` the source address `new` in place of `old`, as a job with
/// every capability of its own namespace can; the host's end of its link keeps answering.
fn readdress(holder: &Holder, old: &str, new: &str, mac: &str) {
    let steps: [Vec<&str>; 4] = [
        vec!["addr", "add", new, "dev", "eth0"],
        vec!["addr", "del", old, "dev", "eth0"],
        vec![
            "route",
            "replace",
            "default",
            "via",
            "198.18.0.1",
            "dev",
            "eth0",
            "onlink",
        ],
        vec![
            "neigh",
            "replace",
            "198.18.0.1",
            "lladdr",
            mac,
            "dev",
            "eth0",
            "nud",
            "permanent",
        ],
    ];
    for args in steps {
        let output = ip_in(&holder.ns, &args);
        assert!(output.status.success(), "ip {args:?}: {}", text(&output));
    }
}

/// Asserts the spoofed-source rule. With the kernel's reverse-path filter off, a job that
/// takes an address outside the pool, and then another run's address of the pool, gets no
/// connection to the allowed endpoint: the firewall's `spoof` rule counts both and the
/// listener sees nothing. The control removes that rule: the first spoof (with a route back
/// to it, as a real attacker would need) then connects and the far host answers a source the
/// host never gave out, and the second is forwarded (the allowance counts it). So the rule is
/// what refused them.
#[test]
#[ignore = "needs root, ip, nft, unshare: see the module docs"]
fn a_job_cannot_send_from_an_address_that_is_not_its_own() {
    let _serial = serial();
    let far_host = Far::new();
    let allowed = Server::tcp(&far_host.ns, far::ALLOWED);
    let egress = Egress::start(allow_list(&["192.0.2.10:9300"], &[])).expect("start");
    let _rp = RpFilter::off();
    let (a, b) = (Holder::new(), Holder::new());
    let _first = egress.attach("a", a.pid()).expect("attach a");
    let _second = egress.attach("b", b.pid()).expect("attach b");
    sysctl("net/ipv4/conf/jsv0/rp_filter", "0");
    let mac = std::fs::read_to_string("/sys/class/net/jsv0/address").expect("mac");
    let mac = mac.trim();

    // Outside the pool.
    readdress(&a, "198.18.0.2/30", "203.0.113.9/32", mac);
    let before = packets("spoof");
    let reached = tcp(&a.ns, far::ALLOWED);
    assert!(matches!(reached, Reach::Failed(_)), "{reached:?}");
    assert!(packets("spoof") > before, "the rule did not count it");
    // Another run's address.
    readdress(&a, "203.0.113.9/32", "198.18.0.6/32", mac);
    let before = packets("spoof");
    let reached = tcp(&a.ns, far::ALLOWED);
    assert!(matches!(reached, Reach::Failed(_)), "{reached:?}");
    assert!(packets("spoof") > before, "the rule did not count it");
    assert_eq!(allowed.accepted(), 0, "a spoofed connection arrived");

    // The control: without the rule.
    delete_rule("spoof");
    ip(&["route", "add", "203.0.113.9/32", "dev", "jsv0"]);
    readdress(&a, "198.18.0.6/32", "203.0.113.9/32", mac);
    let reached = tcp(&a.ns, far::ALLOWED);
    assert!(reached.connected(), "{reached:?}");
    assert_eq!(allowed.accepted(), 1);
    assert_eq!(allowed.peers(), [IpAddr::V4(far::HOST)]);
    readdress(&a, "203.0.113.9/32", "198.18.0.6/32", mac);
    let forwarded = packets("allow-0");
    let _ = tcp(&a.ns, far::ALLOWED);
    assert!(
        packets("allow-0") > forwarded,
        "without the rule another run's address is forwarded"
    );
}
