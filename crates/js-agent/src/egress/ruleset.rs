//! The firewall of the ceiling (`allow-list` or `open`), as nftables text, and the comparison
//! that tells whether what the kernel holds is still what the agent loaded.
//!
//! Two tables, both the agent's alone, both loaded in one `nft -f` transaction that first
//! removes any earlier copy:
//!
//! - `inet js_crun_egress`: the `forward` chain (a filter hook at priority -10, ahead of the
//!   distribution's own forward rules at 0) holds, in order, the acceptance of replies to a
//!   job, the refusal of new traffic to a job, the drop of everything IPv6 from a job, the
//!   refusal of a source address that does not route back through the interface it came in
//!   on (`fib saddr . iif oif missing`: a job's own address and nothing else), the refusal of
//!   `169.254.0.0/16`, one acceptance per resolver (UDP and TCP, port 53) and per
//!   endpoint (a new TCP connection) for an interface in the set `jobifs`, under the ceiling
//!   `open` one acceptance of IPv4 to any address for an interface in the set `openifs`, and
//!   the refusal of everything else from an interface named `jsv*`. The `input` chain refuses
//!   every packet a job sends to the host itself. Every rule counts and carries a comment, so
//!   a refusal can be told to be the rule's doing.
//! - `ip js_crun_egress_nat`: the masquerade of the pool's addresses toward anything outside
//!   the pool, so that an endpoint on another host answers to the host and not to an
//!   address only the host knows.
//!
//! The set `jobifs` holds the names of the interfaces of the `allow-list` runs that are wired
//! now, and `openifs` (rendered under the ceiling `open` only) those of the `open` runs; the
//! only rules that accept a new connection match one of the sets, so an interface named
//! `jsv*` that the agent did not register reaches nothing. Every refusal comes before the
//! `open` acceptance, so an `open` job still cannot reach the metadata range, send IPv6 or a
//! spoofed source, reach another job (`inbound`) or the host (`input`). Under the ceiling
//! `allow-list` the text is what it was before `open` existed.
//!
//! Every rule sits in the host's initial network namespace, which a job cannot reach: the
//! job's own namespace has its end of a veth pair and no firewall at all.

use std::fmt::Write as _;

use serde_json::Value;

use super::config::{Config, Mode};

/// The table of the filter rules.
pub const TABLE: &str = "js_crun_egress";

/// The table of the masquerade rule.
pub const NAT_TABLE: &str = "js_crun_egress_nat";

/// The set of the wired interfaces of `allow-list` runs.
pub const IFACE_SET: &str = "jobifs";

/// The set of the wired interfaces of `open` runs; only the ceiling `open` has it.
pub const OPEN_SET: &str = "openifs";

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
    let open = config.mode() == Mode::Open;
    let _ = write!(
        out,
        "table inet {TABLE} {{\n\tset {IFACE_SET} {{\n\t\ttype ifname\n\t}}\n"
    );
    if open {
        let _ = writeln!(out, "\tset {OPEN_SET} {{\n\t\ttype ifname\n\t}}");
    }
    let _ = write!(
        out,
        "\tchain forward {{\n\
         \t\ttype filter hook forward priority {PRIORITY}; policy accept;\n\
         \t\toifname \"{IF_PREFIX}*\" ct state established,related counter accept comment \"reply\"\n\
         \t\toifname \"{IF_PREFIX}*\" counter drop comment \"inbound\"\n\
         \t\tiifname \"{IF_PREFIX}*\" meta nfproto ipv6 counter drop comment \"ipv6\"\n\
         \t\tiifname \"{IF_PREFIX}*\" meta nfproto ipv4 fib saddr . iif oif missing counter drop comment \"spoof\"\n\
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
    if open {
        let _ = writeln!(
            out,
            "\t\tiifname @{OPEN_SET} meta nfproto ipv4 counter accept comment \"open\""
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
/// the members of the sets `jobifs` and `openifs` (they change with every run) are left out.
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
    #[cfg(target_os = "linux")]
    use crate::egress::Flags;

    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    #[test]
    fn the_ruleset_holds_each_rule_in_its_place() {
        let text = render(&config());
        let at = |needle: &str| text.find(needle).expect(needle);
        assert!(at("delete table inet js_crun_egress") < at("table inet js_crun_egress {"));
        assert!(at("delete table ip js_crun_egress_nat") < at("table ip js_crun_egress_nat {"));
        let order = [
            "comment \"reply\"",
            "comment \"inbound\"",
            "comment \"ipv6\"",
            "comment \"spoof\"",
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

    /// The allow-list of [`config`] as the agent loaded it before the ceiling `open` existed.
    #[cfg(target_os = "linux")]
    const ALLOW_LIST_TEXT: &str = "add table inet js_crun_egress\n\
        delete table inet js_crun_egress\n\
        add table ip js_crun_egress_nat\n\
        delete table ip js_crun_egress_nat\n\
        table inet js_crun_egress {\n\
        \tset jobifs {\n\
        \t\ttype ifname\n\
        \t}\n\
        \tchain forward {\n\
        \t\ttype filter hook forward priority -10; policy accept;\n\
        \t\toifname \"jsv*\" ct state established,related counter accept comment \"reply\"\n\
        \t\toifname \"jsv*\" counter drop comment \"inbound\"\n\
        \t\tiifname \"jsv*\" meta nfproto ipv6 counter drop comment \"ipv6\"\n\
        \t\tiifname \"jsv*\" meta nfproto ipv4 fib saddr . iif oif missing counter drop comment \"spoof\"\n\
        \t\tiifname \"jsv*\" ip daddr 169.254.0.0/16 counter reject with icmp type admin-prohibited comment \"metadata\"\n\
        \t\tiifname @jobifs ip daddr 192.0.2.53 udp dport 53 counter accept comment \"dns-udp-0\"\n\
        \t\tiifname @jobifs ip daddr 192.0.2.53 tcp dport 53 counter accept comment \"dns-tcp-0\"\n\
        \t\tiifname @jobifs ip daddr 192.0.2.10 tcp dport 9300 counter accept comment \"allow-0\"\n\
        \t\tiifname @jobifs ip daddr 203.0.113.0/24 tcp dport 443 counter accept comment \"allow-1\"\n\
        \t\tiifname \"jsv*\" meta nfproto ipv4 counter reject with icmp type admin-prohibited comment \"default-reject\"\n\
        \t\tiifname \"jsv*\" counter drop comment \"default-drop\"\n\
        \t}\n\
        \tchain input {\n\
        \t\ttype filter hook input priority -10; policy accept;\n\
        \t\tiifname \"jsv*\" meta nfproto ipv4 counter reject with icmp type admin-prohibited comment \"input\"\n\
        \t\tiifname \"jsv*\" counter drop comment \"input-other\"\n\
        \t}\n\
        }\n\
        table ip js_crun_egress_nat {\n\
        \tchain postrouting {\n\
        \t\ttype nat hook postrouting priority srcnat; policy accept;\n\
        \t\tip saddr 198.18.0.0/16 ip daddr != 198.18.0.0/16 counter masquerade comment \"masquerade\"\n\
        \t}\n\
        }\n";

    /// Catches a ceiling `allow-list` whose firewall changed when `open` was added: the text
    /// is byte for byte what it was, with no `openifs` set and no `open` rule.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_allow_list_ruleset_is_the_text_it_was() {
        let text = render(&config());
        assert_eq!(text, ALLOW_LIST_TEXT);
        assert!(!text.contains(OPEN_SET));
    }

    /// Catches an `open` ceiling whose acceptance comes before a refusal, one that lets IPv6
    /// through, and one that lets an open job reach the host or another job: the `openifs`
    /// set is declared, its one acceptance is of IPv4 from that set to any address, after the
    /// replies, the inbound refusal (another job's address), the IPv6 drop, the spoof refusal,
    /// the metadata refusal and the allow-list's acceptances, and before the default refusal;
    /// the `input` chain (the host itself) and the masquerade are still there; and an
    /// allow-list job under it still has its acceptances for `jobifs`.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_allows_any_address_except_the_always_denied_ranges() {
        let allow = ["192.0.2.10:9300".to_owned()];
        let dns = ["192.0.2.53".to_owned()];
        let open = Config::from_flags(&Flags {
            mode: Some("open"),
            allow: &allow,
            dns: &dns,
            ip: Some("/usr/bin/true"),
            nft: Some("/usr/bin/true"),
            nsenter: Some("/usr/bin/true"),
            ..Flags::default()
        })
        .expect("a sound configuration");
        let text = render(&open);
        let at = |needle: &str| text.find(needle).expect(needle);
        assert!(at("set jobifs {") < at("set openifs {"));
        assert!(at("set openifs {") < at("chain forward {"));
        let order = [
            "comment \"reply\"",
            "comment \"inbound\"",
            "comment \"ipv6\"",
            "comment \"spoof\"",
            "comment \"metadata\"",
            "comment \"dns-udp-0\"",
            "comment \"allow-0\"",
            "comment \"open\"",
            "comment \"default-reject\"",
            "comment \"default-drop\"",
            "comment \"input\"",
            "comment \"input-other\"",
            "comment \"masquerade\"",
        ];
        let places: Vec<usize> = order.iter().map(|rule| at(rule)).collect();
        assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        let accept = text
            .lines()
            .find(|line| line.contains("comment \"open\""))
            .expect("the open rule");
        assert_eq!(
            accept.trim(),
            "iifname @openifs meta nfproto ipv4 counter accept comment \"open\""
        );
        assert_eq!(text.matches("@openifs").count(), 1, "{text}");
        assert!(text.contains("iifname @jobifs ip daddr 192.0.2.10 tcp dport 9300"));
        // Only the open rule differs from the allow-list of the same flags.
        let listed = Config::from_flags(&Flags {
            mode: Some("allow-list"),
            allow: &allow,
            dns: &dns,
            ip: Some("/usr/bin/true"),
            nft: Some("/usr/bin/true"),
            nsenter: Some("/usr/bin/true"),
            ..Flags::default()
        })
        .expect("a sound configuration");
        let without: Vec<&str> = text
            .lines()
            .filter(|line| !line.contains("openifs") && !line.contains("comment \"open\""))
            .collect();
        let lines = render(&listed);
        let listed_lines: Vec<&str> = lines.lines().collect();
        // The set's own two closing lines (`type ifname`, `}`) are the only other extra.
        assert_eq!(without.len(), listed_lines.len() + 2, "{text}");
    }

    /// Catches a listing compared with counters, handles or the members of the set in it:
    /// the same table read twice, with other counts, other handles and another member, is
    /// the same; a rule that changed is not.
    #[test]
    fn the_comparison_ignores_counts_handles_and_members_and_nothing_else() {
        let listing = |packets: u64, handle: u64, member: &str, port: u64| {
            serde_json::json!({"nftables": [
                {"metainfo": {"version": "1.0.9"}},
                {"table": {"family": "inet", "name": "js_crun_egress", "handle": handle}},
                {"set": {"name": "jobifs", "type": "ifname", "elem": [member], "handle": 2}},
                {"set": {"name": "openifs", "type": "ifname", "elem": [member], "handle": 3}},
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
