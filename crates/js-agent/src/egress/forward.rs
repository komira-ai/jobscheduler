//! The forwards of a job's declared ports (design 5.8, "Ports"), as nftables text, and the
//! reading of them back out of a listing of the two tables.
//!
//! Each declared port `{host, container}` of a wired job is two rules, both commented
//! `fwd-<host>` (the comment prefix [`PREFIX`] is theirs alone):
//!
//! - in the chain `prerouting` of `ip js_crun_egress_nat` (a NAT hook at priority `dstnat`,
//!   which [`super::ruleset::render`] makes empty): `fib daddr type local tcp dport <host>
//!   counter dnat to <job>:<container>`. `fib daddr type local` leaves alone a packet the host
//!   merely routes (one not addressed to the host itself).
//! - in the chain `forward` of `inet js_crun_egress`, ahead of the `inbound` refusal (`insert
//!   rule` puts it first): `oifname "<iface>" ip daddr <job> tcp dport <container> ct status
//!   dnat counter accept`. It names the one interface, the one address and the one port, and
//!   needs the packet to have been DNATed, so traffic aimed straight at the job's address, at
//!   another of its ports or at another job still meets `inbound`.
//!
//! The replies of a forwarded connection leave the job through the static rule
//! `forward-reply` of the ruleset (`ct direction reply ct status dnat`), which no other
//! connection of a job matches.
//!
//! TCP only: a declared port ([`js_types::PortMap`]) carries no protocol. A packet the host
//! itself sends does not pass `prerouting`, so a forward is not reached from the host's own
//! loopback, and that is not promised.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde_json::{Value, json};

use super::ruleset::{NAT_TABLE, TABLE};

/// The comment prefix of the forwards' rules: no other rule's comment starts with it.
pub const PREFIX: &str = "fwd-";

/// The chain of `ip js_crun_egress_nat` that holds the DNAT rules.
pub const NAT_CHAIN: &str = "prerouting";

/// The chain of `inet js_crun_egress` that holds the acceptances.
const FILTER_CHAIN: &str = "forward";

/// One forward: the host's port, the job's address and port behind it, and the host end of
/// the job's veth pair, which the acceptance names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Forward {
    /// The host's port.
    pub host: u16,
    /// The job's address.
    pub job: Ipv4Addr,
    /// The job's port inside its container.
    pub container: u16,
    /// The host end of the job's veth pair (`jsvN`).
    pub iface: String,
}

impl Forward {
    /// The text of the forward's two rules, as [`forward_rules`] gives it.
    #[must_use]
    pub fn rules(&self) -> (String, String) {
        forward_rules(self.host, self.job, self.container, &self.iface)
    }
}

/// The comment of the two rules of the host port `host`: `fwd-<host>`.
#[must_use]
pub fn comment(host: u16) -> String {
    format!("{PREFIX}{host}")
}

/// The `nft -f` lines of the forward from the host's port `host_port` to `job_ip` and
/// `container_port`, for the job whose veth's host end is `iface`: the DNAT rule (added to
/// `prerouting`), then the acceptance (inserted at the head of `forward`, so before
/// `inbound`). Each ends without a newline.
#[must_use]
pub fn forward_rules(
    host_port: u16,
    job_ip: Ipv4Addr,
    container_port: u16,
    iface: &str,
) -> (String, String) {
    let comment = comment(host_port);
    (
        format!(
            "add rule ip {NAT_TABLE} {NAT_CHAIN} fib daddr type local tcp dport {host_port} \
             counter dnat to {job_ip}:{container_port} comment \"{comment}\""
        ),
        format!(
            "insert rule inet {TABLE} {FILTER_CHAIN} oifname \"{iface}\" ip daddr {job_ip} \
             tcp dport {container_port} ct status dnat counter accept comment \"{comment}\""
        ),
    )
}

/// One `nft -f` transaction that adds every rule of `forwards`: the kernel takes all of them
/// or none.
#[must_use]
pub fn transaction(forwards: &[Forward]) -> String {
    forwards
        .iter()
        .map(|forward| {
            let (nat, filter) = forward.rules();
            format!("{nat}\n{filter}\n")
        })
        .collect()
}

/// The forwards of the job whose veth's host end is `id` that `listing` holds whole, sorted:
/// see [`all`].
#[must_use]
pub fn decode(listing: &Value, id: &str) -> Vec<Forward> {
    all(listing)
        .into_iter()
        .filter(|forward| forward.iface == id)
        .collect()
}

/// Every forward `listing` (both tables normalized, `[inet, nat]`, as the host side reads
/// them) holds whole, sorted: for each comment `fwd-<host>`, exactly one DNAT rule in
/// `prerouting` of the NAT table and one acceptance in `forward` of the filter table, each
/// made of exactly the expressions [`forward_rules`] writes (the protocol dependencies nft
/// may list are allowed), that agree on the job's address and port, under the comment of
/// their own host port. A comment with anything else (a rule missing, twice, changed, in
/// another chain) gives no forward.
#[must_use]
pub fn all(listing: &Value) -> Vec<Forward> {
    let mut halves: BTreeMap<&str, Vec<Option<Half>>> = BTreeMap::new();
    for (nat, rule) in rules(listing) {
        if let Some(comment) = forward_comment(rule) {
            halves.entry(comment).or_default().push(half(nat, rule));
        }
    }
    let mut forwards: Vec<Forward> = halves
        .into_iter()
        .filter_map(|(comment, halves)| whole(comment, &halves))
        .collect();
    forwards.sort();
    forwards
}

/// `listing` without the rules of the forwards, and how many there were; none when an
/// acceptance of the chain `forward` comes after a rule of that chain that is not one (they
/// are inserted ahead of the `inbound` refusal, and behind it they would accept nothing).
#[must_use]
pub fn split(listing: &Value) -> Option<(Value, usize)> {
    let order: Vec<bool> = rules(listing)
        .filter(|(nat, rule)| {
            !nat && rule.get("chain").and_then(Value::as_str) == Some(FILTER_CHAIN)
        })
        .map(|(_, rule)| forward_comment(rule).is_some())
        .collect();
    let mut count = 0;
    let rest: Vec<Value> = listing
        .as_array()?
        .iter()
        .map(|table| {
            let items = table.as_array().map(Vec::as_slice).unwrap_or_default();
            let kept: Vec<Value> = items
                .iter()
                .filter(|item| item.get("rule").and_then(forward_comment).is_none())
                .cloned()
                .collect();
            count += items.len() - kept.len();
            Value::Array(kept)
        })
        .collect();
    // In order: every forward's acceptance (`true`) before every other rule (`false`).
    let in_order = order.windows(2).all(|pair| pair[0] >= pair[1]);
    in_order.then_some((Value::Array(rest), count))
}

/// The rules of `listing` (`[inet, nat]`), in order, each with whether it is the NAT table's.
fn rules(listing: &Value) -> impl Iterator<Item = (bool, &Value)> {
    let tables = listing.as_array().map(Vec::as_slice).unwrap_or_default();
    tables.iter().enumerate().flat_map(|(n, table)| {
        let items = table.as_array().map(Vec::as_slice).unwrap_or_default();
        items
            .iter()
            .filter_map(|item| item.get("rule"))
            .map(move |rule| (n == 1, rule))
    })
}

/// The comment of `rule` when it is a forward's (`fwd-...`).
fn forward_comment(rule: &Value) -> Option<&str> {
    rule.get("comment")
        .and_then(Value::as_str)
        .filter(|comment| comment.starts_with(PREFIX))
}

/// What one of a forward's two rules says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Half {
    /// The DNAT rule: the host's port to the job's address and port.
    Nat {
        host: u16,
        job: Ipv4Addr,
        container: u16,
    },
    /// The acceptance: the interface, the job's address and port.
    Filter {
        iface: String,
        job: Ipv4Addr,
        container: u16,
    },
}

/// The forward of `comment` when its rules (`halves`, the filter table's first) are exactly
/// one acceptance and one DNAT rule that agree, under the comment of their host port.
fn whole(comment: &str, halves: &[Option<Half>]) -> Option<Forward> {
    let [
        Some(Half::Filter {
            iface,
            job: to,
            container: port,
        }),
        Some(Half::Nat {
            host,
            job,
            container,
        }),
    ] = halves
    else {
        return None;
    };
    let agree = (comment.to_owned(), *to, *port) == (self::comment(*host), *job, *container);
    agree.then(|| Forward {
        host: *host,
        job: *job,
        container: *container,
        iface: iface.clone(),
    })
}

/// What `rule`, a forward's rule of the NAT table (`nat`) or of the filter table, says; none
/// when it is not in its chain or is not exactly what [`forward_rules`] writes.
fn half(nat: bool, rule: &Value) -> Option<Half> {
    let chain = rule.get("chain")?.as_str()?;
    let exprs = rule.get("expr")?.as_array()?;
    let pieces: Vec<Piece> = exprs.iter().map(piece).collect::<Option<_>>()?;
    let pieces: Vec<Piece> = pieces
        .into_iter()
        .filter(|piece| *piece != Piece::Implied)
        .collect();
    match (nat, chain, pieces.as_slice()) {
        (
            true,
            NAT_CHAIN,
            [
                Piece::Local,
                Piece::Dport(host),
                Piece::Counter,
                Piece::Dnat(job, container),
            ],
        ) => Some(Half::Nat {
            host: *host,
            job: *job,
            container: *container,
        }),
        (
            false,
            FILTER_CHAIN,
            [
                Piece::Oif(iface),
                Piece::Daddr(job),
                Piece::Dport(container),
                Piece::Dnatted,
                Piece::Counter,
                Piece::Accept,
            ],
        ) => Some(Half::Filter {
            iface: iface.clone(),
            job: *job,
            container: *container,
        }),
        _ => None,
    }
}

/// One expression of a forward's rule.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// `fib daddr type local`.
    Local,
    /// `oifname "<name>"`.
    Oif(String),
    /// `ip daddr <addr>`.
    Daddr(Ipv4Addr),
    /// `tcp dport <port>`.
    Dport(u16),
    /// `ct status dnat`.
    Dnatted,
    /// `counter`.
    Counter,
    /// `accept`.
    Accept,
    /// `dnat to <addr>:<port>`.
    Dnat(Ipv4Addr, u16),
    /// A dependency nft may list for `tcp` or `ip` (`meta l4proto tcp`, `ip protocol tcp`,
    /// `meta nfproto ipv4`): it narrows nothing the rule does not already.
    Implied,
}

/// What the expression `expr` is; none for anything a forward's rule does not hold.
fn piece(expr: &Value) -> Option<Piece> {
    let (key, body) = expr
        .as_object()
        .filter(|one| one.len() == 1)?
        .iter()
        .next()?;
    match key.as_str() {
        "counter" => Some(Piece::Counter),
        "accept" => body.is_null().then_some(Piece::Accept),
        "dnat" => dnat(body),
        "match" => matched(body),
        _ => None,
    }
}

/// `dnat to <addr>:<port>`, with no other field than the family `ip`.
fn dnat(body: &Value) -> Option<Piece> {
    let fields = body.as_object()?;
    let known = fields
        .keys()
        .all(|key| ["addr", "port", "family"].contains(&key.as_str()));
    let family = fields.get("family").is_none_or(|family| family == "ip");
    let addr = fields.get("addr")?.as_str()?.parse().ok()?;
    let port = port(fields.get("port")?)?;
    ([known, family] == [true, true]).then_some(Piece::Dnat(addr, port))
}

/// A port of a listing: a number from 0 to 65535.
fn port(value: &Value) -> Option<u16> {
    value.as_u64().and_then(|port| u16::try_from(port).ok())
}

/// What a match compares, of the ones a forward's rule holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    Oif,
    Daddr,
    Dport,
    Status,
    Local,
    Proto,
    Family,
}

/// What the left side of a match is.
fn subject(left: &Value) -> Option<Subject> {
    [
        (json!({"meta": {"key": "oifname"}}), Subject::Oif),
        (
            json!({"payload": {"protocol": "ip", "field": "daddr"}}),
            Subject::Daddr,
        ),
        (
            json!({"payload": {"protocol": "tcp", "field": "dport"}}),
            Subject::Dport,
        ),
        (json!({"ct": {"key": "status"}}), Subject::Status),
        (
            json!({"fib": {"result": "type", "flags": ["daddr"]}}),
            Subject::Local,
        ),
        (
            json!({"fib": {"result": "type", "flags": "daddr"}}),
            Subject::Local,
        ),
        (json!({"meta": {"key": "l4proto"}}), Subject::Proto),
        (
            json!({"payload": {"protocol": "ip", "field": "protocol"}}),
            Subject::Proto,
        ),
        (json!({"meta": {"key": "nfproto"}}), Subject::Family),
    ]
    .into_iter()
    .find_map(|(known, subject)| (known == *left).then_some(subject))
}

/// The match `body` (`{op, left, right}`) as a piece.
fn matched(body: &Value) -> Option<Piece> {
    let op = body.get("op")?.as_str()?;
    let right = body.get("right")?;
    let piece = match (subject(body.get("left")?)?, op) {
        (Subject::Oif, "==") => Piece::Oif(right.as_str()?.to_owned()),
        (Subject::Daddr, "==") => Piece::Daddr(right.as_str()?.parse().ok()?),
        (Subject::Dport, "==") => Piece::Dport(port(right)?),
        (Subject::Status, "in" | "==") => flag(right, "dnat", Piece::Dnatted)?,
        (Subject::Local, "==") => flag(right, "local", Piece::Local)?,
        (Subject::Proto, "==") => flag(right, "tcp", Piece::Implied)?,
        (Subject::Family, "==") => flag(right, "ipv4", Piece::Implied)?,
        _ => return None,
    };
    Some(piece)
}

/// `piece` when `value` is `name`, alone or as a list of one.
fn flag(value: &Value, name: &str, piece: Piece) -> Option<Piece> {
    [json!(name), json!([name])]
        .contains(value)
        .then_some(piece)
}

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
mod tests;
