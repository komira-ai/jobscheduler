//! The rules of a forward and the rules around them as `nft -j list table` prints them, for
//! the unit tests of this module and of the host side.

use serde_json::{Value, json};

/// The DNAT rule of the forward of `host` to `job`:`container`, with the handle `handle`.
pub(crate) fn nat_rule(host: u16, job: &str, container: u16, handle: u64) -> Value {
    json!({"rule": {
        "family": "ip", "table": "js_crun_egress_nat", "chain": "prerouting",
        "handle": handle, "comment": format!("fwd-{host}"),
        "expr": [
            {"match": {"op": "!=", "left": {"meta": {"key": "iifname"}}, "right": "jsv*"}},
            {"match": {"op": "==", "left": {"fib": {"result": "type", "flags": ["daddr"]}}, "right": "local"}},
            {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": host}},
            {"counter": {"packets": 0, "bytes": 0}},
            {"dnat": {"addr": job, "port": container}}
        ]
    }})
}

/// The acceptance of the forward of `host` to `job`:`container` through `iface`, with the
/// handle `handle`.
pub(crate) fn filter_rule(host: u16, iface: &str, job: &str, container: u16, handle: u64) -> Value {
    json!({"rule": {
        "family": "inet", "table": "js_crun_egress", "chain": "forward",
        "handle": handle, "comment": format!("fwd-{host}"),
        "expr": [
            {"match": {"op": "==", "left": {"meta": {"key": "oifname"}}, "right": iface}},
            {"match": {"op": "==", "left": {"payload": {"protocol": "ip", "field": "daddr"}}, "right": job}},
            {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": container}},
            {"match": {"op": "in", "left": {"ct": {"key": "status"}}, "right": "dnat"}},
            {"counter": {"packets": 0, "bytes": 0}},
            {"accept": null}
        ]
    }})
}

/// The `inbound` refusal of the chain `forward`, with the handle `handle`.
pub(crate) fn inbound(handle: u64) -> Value {
    json!({"rule": {
        "family": "inet", "table": "js_crun_egress", "chain": "forward",
        "handle": handle, "comment": "inbound",
        "expr": [
            {"match": {"op": "==", "left": {"meta": {"key": "oifname"}}, "right": "jsv*"}},
            {"counter": {"packets": 0, "bytes": 0}},
            {"drop": null}
        ]
    }})
}

/// The text `nft -j list table <family> <name>` prints for a table that holds `items`.
pub(crate) fn printed(family: &str, name: &str, items: &[Value]) -> String {
    let mut all = vec![
        json!({"metainfo": {"version": "1.0.9"}}),
        json!({"table": {"family": family, "name": name, "handle": 1}}),
    ];
    all.extend_from_slice(items);
    json!({ "nftables": all }).to_string()
}

/// Both tables holding `inet` and `nat`, as the host side reads them back: normalized,
/// `[inet, nat]`.
pub(crate) fn listing(inet: &[Value], nat: &[Value]) -> Value {
    let normalized = |family: &str, name: &str, items: &[Value]| {
        super::super::ruleset::normalize(&printed(family, name, items)).expect("nft json")
    };
    json!([
        normalized("inet", "js_crun_egress", inet),
        normalized("ip", "js_crun_egress_nat", nat)
    ])
}

/// `rule` with the value at `pointer` (a JSON pointer) set to `value`: a field of an object
/// is replaced or added, an item of a list replaced.
pub(crate) fn with(mut rule: Value, pointer: &str, value: Value) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("a pointer");
    match rule.pointer_mut(parent).expect("the parent is there") {
        Value::Array(items) => items[key.parse::<usize>().expect("an index")] = value,
        other => other[key] = value,
    }
    rule
}
