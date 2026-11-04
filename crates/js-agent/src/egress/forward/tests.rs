//! The forwards' text, and what a listing reads back as.

use serde_json::json;

use super::fixture::{filter_rule, inbound, listing, nat_rule, with};
use super::*;

const JOB: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);

/// No forward.
fn none() -> Vec<Forward> {
    Vec::new()
}

fn forward(host: u16, job: Ipv4Addr, container: u16, iface: &str) -> Forward {
    Forward {
        host,
        job,
        container,
        iface: iface.to_owned(),
    }
}

/// Catches a forward that reaches more than its one destination and port, or that accepts a
/// packet that was not DNATed: the DNAT rule rewrites only a packet addressed to the host
/// itself on the one host port, to the one address and port; the acceptance names the one
/// interface, the one address, the one port and needs `ct status dnat`; both carry the
/// comment `fwd-<host>`, and nothing in either is a range, a set or a prefix. The
/// transaction holds both rules of every forward, the DNAT rule first.
#[test]
fn forward_rules_name_one_destination_and_one_port() {
    let (nat, filter) = forward_rules(8080, JOB, 80, "jsv0");
    assert_eq!(
        nat,
        "add rule ip js_crun_egress_nat prerouting fib daddr type local tcp dport 8080 \
         counter dnat to 198.18.0.2:80 comment \"fwd-8080\""
    );
    assert_eq!(
        filter,
        "insert rule inet js_crun_egress forward oifname \"jsv0\" ip daddr 198.18.0.2 \
         tcp dport 80 ct status dnat counter accept comment \"fwd-8080\""
    );
    for text in [&nat, &filter] {
        for wide in ["/", "{", "@", "*", "-80", "8080-"] {
            assert!(!text.contains(wide), "{wide} in {text}");
        }
    }
    assert_eq!(comment(8080), "fwd-8080");
    let both = [
        forward(8080, JOB, 80, "jsv0"),
        forward(9000, JOB, 9001, "jsv0"),
    ];
    let (nat_b, filter_b) = both[1].rules();
    assert_eq!(
        transaction(&both),
        format!("{nat}\n{filter}\n{nat_b}\n{filter_b}\n")
    );
    assert!(nat_b.contains("dport 9000"), "{nat_b}");
    assert!(nat_b.contains("198.18.0.2:9001"), "{nat_b}");
    assert_eq!(transaction(&[]), "");
}

/// Catches an acceptance that would sit behind the `inbound` refusal, where it accepts
/// nothing: the rule is inserted (at the head of the chain), never appended, and a listing
/// in which a forward's acceptance follows `inbound` is out of order, while one in which it
/// leads is split into the rest of the tables and the count of the forwards' rules.
#[test]
fn the_filter_forward_precedes_the_inbound_drop() {
    let (_, filter) = forward_rules(8080, JOB, 80, "jsv0");
    assert!(
        filter.starts_with("insert rule inet js_crun_egress forward "),
        "{filter}"
    );
    let nat = [nat_rule(8080, "198.18.0.2", 80, 9)];
    let first = listing(
        &[filter_rule(8080, "jsv0", "198.18.0.2", 80, 8), inbound(3)],
        &nat,
    );
    let (rest, count) = split(&first).expect("in order");
    assert_eq!(count, 2);
    assert_eq!(rest, listing(&[inbound(3)], &[]));
    let behind = listing(
        &[inbound(3), filter_rule(8080, "jsv0", "198.18.0.2", 80, 8)],
        &nat,
    );
    assert_eq!(split(&behind), None);
    // A rule of another chain, and the forwards' rules of the NAT table, have no order to
    // keep with `inbound`.
    let input = json!({"rule": {"chain": "input", "comment": "input", "expr": []}});
    let other = listing(
        &[
            filter_rule(8080, "jsv0", "198.18.0.2", 80, 8),
            inbound(3),
            input,
        ],
        &nat,
    );
    assert_eq!(split(&other).map(|(_, count)| count), Some(2));
    assert_eq!(split(&json!({})), None, "not a listing");
}

/// Catches a decoder that ignores a field: a listing of two jobs' forwards decodes, for each
/// job's interface, to exactly the forwards that made it, and with any one field of either
/// rule changed (the address, a port, the interface, the comment) to something else or to
/// nothing.
#[test]
fn a_listing_decodes_back_to_the_forwards_that_made_it() {
    let made = [
        forward(8080, JOB, 80, "jsv0"),
        forward(9090, Ipv4Addr::new(198, 18, 0, 6), 90, "jsv1"),
    ];
    let read = listing(
        &[
            filter_rule(9090, "jsv1", "198.18.0.6", 90, 10),
            filter_rule(8080, "jsv0", "198.18.0.2", 80, 8),
            inbound(3),
        ],
        &[
            nat_rule(8080, "198.18.0.2", 80, 9),
            nat_rule(9090, "198.18.0.6", 90, 11),
        ],
    );
    assert_eq!(decode(&read, "jsv0"), [made[0].clone()]);
    assert_eq!(decode(&read, "jsv1"), [made[1].clone()]);
    assert_eq!(decode(&read, "jsv2"), none());
    assert_eq!(all(&read), made);

    let one = |filter: Value, nat: Value| all(&listing(&[filter, inbound(3)], &[nat]));
    let filter = || filter_rule(8080, "jsv0", "198.18.0.2", 80, 8);
    let nat = || nat_rule(8080, "198.18.0.2", 80, 9);
    assert_eq!(one(filter(), nat()), [made[0].clone()]);
    let daddr = with(filter(), "/rule/expr/1/match/right", json!("198.18.0.9"));
    assert_eq!(
        one(daddr, nat()),
        none(),
        "another address in the acceptance"
    );
    let to = with(nat(), "/rule/expr/3/dnat/addr", json!("198.18.0.9"));
    assert_eq!(
        one(filter(), to),
        none(),
        "another address in the DNAT rule"
    );
    let port = with(filter(), "/rule/expr/2/match/right", json!(81));
    assert_eq!(one(port, nat()), none(), "another port in the acceptance");
    let port = with(nat(), "/rule/expr/3/dnat/port", json!(81));
    assert_eq!(one(filter(), port), none(), "another port in the DNAT rule");
    let host = with(nat(), "/rule/expr/1/match/right", json!(8081));
    assert_eq!(one(filter(), host), none(), "a host port not its comment's");
    let iface = with(filter(), "/rule/expr/0/match/right", json!("jsv7"));
    assert_eq!(
        one(iface, nat()),
        [forward(8080, JOB, 80, "jsv7")],
        "the interface is read"
    );
}

/// Catches a decoder that reads a rule wider or other than a forward's as one: each change
/// to either rule below (an operator, a flag, a verdict, an expression added, a chain, a
/// field of the DNAT, a port out of range, a rule missing or twice) leaves no forward, and
/// each way nft may spell the same rule (a flag alone or as a list of one, the protocol
/// dependencies, the family of the DNAT) still reads as the forward.
#[test]
fn only_the_exact_rules_read_as_a_forward() {
    let filter = || filter_rule(8080, "jsv0", "198.18.0.2", 80, 8);
    let nat = || nat_rule(8080, "198.18.0.2", 80, 9);
    let one = |filter: Value, nat: Value| all(&listing(&[filter], &[nat]));
    let made = [forward(8080, JOB, 80, "jsv0")];

    let filters = [
        ("/rule/expr/0/match/op", json!("!=")),
        ("/rule/expr/0/match/op", json!(5)),
        ("/rule/expr/0/match/right", json!(7)),
        (
            "/rule/expr/0/match/left",
            json!({"meta": {"key": "iifname"}}),
        ),
        ("/rule/expr/0/match", json!({"op": "=="})),
        ("/rule/expr/1/match/right", json!("nope")),
        ("/rule/expr/2/match/right", json!(70000)),
        ("/rule/expr/3/match/right", json!("snat")),
        ("/rule/expr/4", json!("counter")),
        ("/rule/expr/4", json!({"counter": null, "log": null})),
        ("/rule/expr/5", json!({"drop": null})),
        ("/rule/expr/5", json!({"accept": 1})),
        ("/rule/expr", json!("none")),
        ("/rule/chain", json!("input")),
        ("/rule/comment", json!("fwd-8081")),
    ];
    for (pointer, value) in filters {
        let changed = with(filter(), pointer, value.clone());
        assert_eq!(one(changed, nat()), none(), "filter {pointer} = {value}");
    }
    let nats = [
        ("/rule/expr/0/match/right", json!("unicast")),
        ("/rule/expr/1/match/op", json!("in")),
        ("/rule/expr/3/dnat", json!("x")),
        ("/rule/expr/3/dnat/addr", json!("x")),
        ("/rule/expr/3/dnat/port", json!("80")),
        ("/rule/expr/3/dnat/family", json!("ip6")),
        ("/rule/expr/3/dnat/flags", json!("persistent")),
        ("/rule/chain", json!("postrouting")),
    ];
    for (pointer, value) in nats {
        let changed = with(nat(), pointer, value.clone());
        assert_eq!(one(filter(), changed), none(), "nat {pointer} = {value}");
    }
    let extra = with(
        filter(),
        "/rule/expr/5",
        json!([{"accept": null}, {"accept": null}]),
    );
    assert_eq!(one(extra, nat()), none(), "an expression that is a list");
    let twice = all(&listing(&[filter(), filter()], &[nat()]));
    assert_eq!(twice, none(), "an acceptance twice");
    assert_eq!(all(&listing(&[filter()], &[])), none(), "no DNAT rule");
    assert_eq!(all(&listing(&[], &[nat()])), none(), "no acceptance");

    let tcp = json!({"match": {"op": "==", "left": {"meta": {"key": "l4proto"}}, "right": "tcp"}});
    let ipv4 =
        json!({"match": {"op": "==", "left": {"meta": {"key": "nfproto"}}, "right": "ipv4"}});
    let protocol = json!({"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "protocol"}}, "right": ["tcp"]}});
    let spelled = |mut rule: Value, before: &[Value]| {
        let exprs = rule
            .pointer_mut("/rule/expr")
            .and_then(Value::as_array_mut)
            .expect("expressions");
        for (n, expr) in before.iter().enumerate() {
            exprs.insert(n, expr.clone());
        }
        rule
    };
    let filter_spelled = with(
        with(
            spelled(filter(), &[ipv4.clone(), tcp.clone()]),
            "/rule/expr/5/match/op",
            json!("=="),
        ),
        "/rule/expr/5/match/right",
        json!(["dnat"]),
    );
    let nat_spelled = with(
        with(
            spelled(nat(), &[protocol]),
            "/rule/expr/1/match/left/fib/flags",
            json!("daddr"),
        ),
        "/rule/expr/4/dnat/family",
        json!("ip"),
    );
    assert_eq!(one(filter_spelled, nat_spelled), made);
    let udp = with(tcp, "/match/right", json!("udp"));
    assert_eq!(one(spelled(filter(), &[udp]), nat()), none(), "udp");
    let ipv6 = with(ipv4, "/match/right", json!("ipv6"));
    assert_eq!(one(spelled(filter(), &[ipv6]), nat()), none(), "ipv6");
}
