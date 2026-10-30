//! The firewall of an allow-list, as nftables text, and the comparison that tells whether
//! what the kernel holds is still what the agent loaded.
//!
//! Two tables, both the agent's alone, both loaded in one `nft -f` transaction that first
//! removes any earlier copy:
//!
//! - `inet js_egress`: the `forward` chain (a filter hook at priority -10, ahead of the
//!   distribution's own forward rules at 0) holds, in order, the acceptance of replies to a
//!   job, the refusal of new traffic to a job, the drop of everything IPv6 from a job, the
//!   refusal of `169.254.0.0/16`, one acceptance per resolver (UDP and TCP, port 53) and per
//!   endpoint (a new TCP connection) for an interface in the set `jobifs`, and the refusal
//!   of everything else from an interface named `jsv*`. The `input` chain refuses every
//!   packet a job sends to the host itself. Every rule counts and carries a comment, so a
//!   refusal can be told to be the rule's doing.
//! - `ip js_egress_nat`: the masquerade of the pool's addresses toward anything outside
//!   the pool, so that an endpoint on another host answers to the host and not to an
//!   address only the host knows.
//!
//! The set `jobifs` holds the names of the interfaces of the runs that are wired now; the
//! only rule that accepts a new connection matches that set, so an interface named `jsv*`
//! that the agent did not register reaches nothing.
//!
//! Every rule sits in the host's initial network namespace, which a job cannot reach: the
//! job's own namespace has its end of a veth pair and no firewall at all.

use std::fmt::Write as _;

use serde_json::Value;

use super::config::Config;

/// The table of the filter rules.
pub const TABLE: &str = "js_egress";

/// The table of the masquerade rule.
pub const NAT_TABLE: &str = "js_egress_nat";

/// The set of the wired interfaces.
pub const IFACE_SET: &str = "jobifs";

/// The name every host end of a run's veth pair starts with (the rules match `jsv*`).
pub const IF_PREFIX: &str = "jsv";

/// The priority of the filter chains: before the distribution's forward chains at 0.
const PRIORITY: i32 = -10;

/// The text `nft -f` loads for `config`: the two tables, replacing any earlier copy.
#[must_use]
pub fn render(config: &Config) -> String {
    let pool = config.pool().text();
    let mut out = String::new();
    // `add` then `delete` makes the replacement work whether or not the table exists.
    let _ = write!(
        out,
        "add table inet {TABLE}\ndelete table inet {TABLE}\n\
         add table ip {NAT_TABLE}\ndelete table ip {NAT_TABLE}\n"
    );
    let _ = write!(
        out,
        "table inet {TABLE} {{\n\
         \tset {IFACE_SET} {{\n\t\ttype ifname\n\t}}\n\
         \tchain forward {{\n\
         \t\ttype filter hook forward priority {PRIORITY}; policy accept;\n\
         \t\toifname \"{IF_PREFIX}*\" ct state established,related counter accept comment \"reply\"\n\
         \t\toifname \"{IF_PREFIX}*\" counter drop comment \"inbound\"\n\
         \t\tiifname \"{IF_PREFIX}*\" meta nfproto ipv6 counter drop comment \"ipv6\"\n\
         \t\tiifname \"{IF_PREFIX}*\" ip daddr 169.254.0.0/16 counter reject with icmp type admin-prohibited comment \"metadata\"\n"
    );
    for (n, server) in config.dns().iter().enumerate() {
        for proto in ["udp", "tcp"] {
            let _ = writeln!(
                out,
                "\t\tiifname @{IFACE_SET} ip daddr {server} {proto} dport 53 counter accept comment \"dns-{proto}-{n}\""
            );
        }
    }
    for (n, allow) in config.allow().iter().enumerate() {
        let net = match allow.prefix_len() {
            32 => allow.addr().to_string(),
            len => format!("{}/{len}", allow.addr()),
        };
        let _ = writeln!(
            out,
            "\t\tiifname @{IFACE_SET} ip daddr {net} tcp dport {} counter accept comment \"allow-{n}\"",
            allow.port()
        );
    }
    let _ = write!(
        out,
        "\t\tiifname \"{IF_PREFIX}*\" meta nfproto ipv4 counter reject with icmp type admin-prohibited comment \"default-reject\"\n\
         \t\tiifname \"{IF_PREFIX}*\" counter drop comment \"default-drop\"\n\
         \t}}\n\
         \tchain input {{\n\
         \t\ttype filter hook input priority {PRIORITY}; policy accept;\n\
         \t\tiifname \"{IF_PREFIX}*\" meta nfproto ipv4 counter reject with icmp type admin-prohibited comment \"input\"\n\
         \t\tiifname \"{IF_PREFIX}*\" counter drop comment \"input-other\"\n\
         \t}}\n}}\n\
         table ip {NAT_TABLE} {{\n\
         \tchain postrouting {{\n\
         \t\ttype nat hook postrouting priority srcnat; policy accept;\n\
         \t\tip saddr {pool} ip daddr != {pool} counter masquerade comment \"masquerade\"\n\
         \t}}\n}}\n"
    );
    out
}

/// What `nft -j list table ...` printed, reduced to what must not change: the structure of
/// the tables, chains and rules. Handles, the version line, the packet and byte counts and
/// the members of the set `jobifs` (they change with every run) are left out.
///
/// # Errors
///
/// `json` is not what `nft -j` prints.
pub fn normalize(json: &str) -> Result<Value, String> {
    let mut value: Value =
        serde_json::from_str(json).map_err(|why| format!("unreadable nft output: {why}"))?;
    let items = value
        .get_mut("nftables")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "nft output has no nftables list".to_owned())?;
    items.retain(|item| item.get("metainfo").is_none());
    for item in items.iter_mut() {
        if let Some(set) = item.get_mut("set").and_then(Value::as_object_mut) {
            set.remove("elem");
        }
        strip(item);
    }
    Ok(Value::Array(std::mem::take(items)))
}

/// Removes handles and zeroes counters, all the way down.
fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("handle");
            if let Some(counter) = map.get_mut("counter").and_then(Value::as_object_mut) {
                for field in ["packets", "bytes"] {
                    if counter.contains_key(field) {
                        counter.insert(field.to_owned(), Value::from(0));
                    }
                }
            }
            map.values_mut().for_each(strip);
        }
        Value::Array(list) => list.iter_mut().for_each(strip),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::Flags;

    fn config() -> Config {
        let allow = [
            "192.0.2.10:9300".to_owned(),
            "203.0.113.0/24:443".to_owned(),
        ];
        let dns = ["192.0.2.53".to_owned()];
        Config::from_flags(&Flags {
            mode: Some("allow-list"),
            allow: &allow,
            dns: &dns,
            pool: Some("198.18.0.0/16"),
            ip: Some("/usr/bin/true"),
            nft: Some("/usr/bin/true"),
            ..Flags::default()
        })
        .expect("a sound configuration")
    }

    /// Catches a ruleset that lacks what the egress modes promise: the order of the rules
    /// (replies and the refusal of inbound first, IPv6 and the metadata range before any
    /// allowance, the default refusal last), one allowance for each endpoint and resolver
    /// and nothing for a name the configuration did not give, the input refusal, the
    /// masquerade of the pool, and the replacement of an earlier copy.
    #[test]
    fn the_ruleset_holds_each_rule_in_its_place() {
        let text = render(&config());
        let at = |needle: &str| text.find(needle).expect(needle);
        assert!(at("delete table inet js_egress") < at("table inet js_egress {"));
        assert!(at("delete table ip js_egress_nat") < at("table ip js_egress_nat {"));
        let order = [
            "comment \"reply\"",
            "comment \"inbound\"",
            "comment \"ipv6\"",
            "comment \"metadata\"",
            "comment \"dns-udp-0\"",
            "comment \"dns-tcp-0\"",
            "comment \"allow-0\"",
            "comment \"allow-1\"",
            "comment \"default-reject\"",
            "comment \"default-drop\"",
            "comment \"input\"",
            "comment \"masquerade\"",
        ];
        let places: Vec<usize> = order.iter().map(|rule| at(rule)).collect();
        assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(text.contains("ip daddr 192.0.2.10 tcp dport 9300 counter accept"));
        assert!(text.contains("ip daddr 203.0.113.0/24 tcp dport 443 counter accept"));
        assert!(text.contains("ip daddr 192.0.2.53 udp dport 53 counter accept"));
        assert!(
            text.contains("ip saddr 198.18.0.0/16 ip daddr != 198.18.0.0/16 counter masquerade")
        );
        assert_eq!(text.matches("counter accept").count(), 5, "{text}");
        // Every accepting rule of a new connection is for a registered interface.
        for line in text
            .lines()
            .filter(|l| l.contains("dport") && l.contains("accept"))
        {
            assert!(line.contains("iifname @jobifs"), "{line}");
        }
    }

    /// Catches a listing compared with counters, handles or the members of the set in it:
    /// the same table read twice, with other counts, other handles and another member, is
    /// the same; a rule that changed is not.
    #[test]
    fn the_comparison_ignores_counts_handles_and_members_and_nothing_else() {
        let listing = |packets: u64, handle: u64, member: &str, port: u64| {
            serde_json::json!({"nftables": [
                {"metainfo": {"version": "1.0.9"}},
                {"table": {"family": "inet", "name": "js_egress", "handle": handle}},
                {"set": {"name": "jobifs", "type": "ifname", "elem": [member], "handle": 2}},
                {"rule": {"chain": "forward", "handle": handle + 1, "expr": [
                    {"match": {"op": "==", "left": {"meta": {"key": "l4proto"}}, "right": "tcp"}},
                    {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": "dport"}}, "right": port}},
                    {"counter": {"packets": packets, "bytes": packets * 60}},
                    {"counter": {"packets": packets}},
                    {"accept": null}]}},
            ]})
            .to_string()
        };
        let base = normalize(&listing(0, 1, "jsv0", 443)).expect("listing");
        assert_eq!(
            base,
            normalize(&listing(9, 40, "jsv7", 443)).expect("listing")
        );
        assert_ne!(
            base,
            normalize(&listing(0, 1, "jsv0", 444)).expect("listing")
        );
        assert!(base.to_string().contains("\"packets\":0"));
        assert!(!base.to_string().contains("jsv0"));
        assert!(normalize("not json").is_err());
        assert!(normalize("{}").is_err());
    }
}
