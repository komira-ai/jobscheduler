//! The egress check: before a contained job is created on a named network, the agent
//! confirms that the host's firewall limits that network to the endpoints it was told
//! (`--container-egress-allow`). The firewall itself is set up by an operator
//! (`deploy/js-agent/egress-network.sh`); the agent only reads it back, through the
//! command runner and `nft -j` (JSON, no text scraping), and refuses the job when what is
//! loaded is not exactly what the script writes for those endpoints.
//!
//! What is compared, for the network's bridge `B`, its one IPv4 subnet `S` and the allowed
//! endpoints: the network is a bridge with DNS off, no IPv6 and a one IPv4 subnet that no
//! endpoint lies in; `br_netfilter` is active (or traffic between two containers of the
//! bridge never reaches the hooks); and in the table `inet js_egress` the `forward` chain
//! (a filter chain on that hook at priority -10) holds, in order, the drop of everything
//! IPv6 from `B`, the drop of `169.254.0.0/16`, the accept of `established,related`, one
//! accept of a new TCP connection from `S` to each endpoint (in any order), and the final
//! drop of everything else from `B`; the `input` chain holds the same without the metadata
//! drop. Anything else (a rule missing, an extra one, a chain
//! that is not a hooked filter chain, a rule in another chain of the table, a rule the
//! parser does not know) is a mismatch. A mismatch never carries nft's output.

use std::fmt;
use std::net::Ipv4Addr;

use serde_json::Value;

use crate::facts::CommandRunner;

/// The table the script installs.
pub const TABLE: &str = "js_egress";

/// The most `--container-egress-allow` entries accepted.
pub const ALLOW_MAX: usize = 16;

/// The program the check runs when `--container-nft` is not given.
pub const DEFAULT_NFT: &str = "/usr/sbin/nft";

/// The link-local range that is always dropped: cloud metadata.
const LINK_LOCAL: (u32, u32) = (0xA9FE_0000, 0xA9FE_FFFF);

/// The sysctl that tells whether bridged traffic goes through the IP hooks.
const BR_NETFILTER: &str = "/proc/sys/net/bridge/bridge-nf-call-iptables";

/// The priority of the table's chains.
const PRIORITY: i64 = -10;

/// A TCP endpoint containers may open: an address or a CIDR of `/24` or narrower, and a
/// port. The same grammar as the script's `--allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allow {
    addr: Ipv4Addr,
    len: u8,
    port: u16,
}

impl Allow {
    /// `IP:PORT` or `CIDR:PORT`: IPv4, a prefix of 24 to 32 with no host bits set, a
    /// port above zero, and nothing that touches `169.254.0.0/16`.
    pub fn parse(text: &str) -> Option<Self> {
        let (net, port) = text.rsplit_once(':')?;
        let port: u16 = port
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| port.parse().ok())
            .flatten()
            .filter(|port| *port > 0)?;
        let (addr, len) = match net.split_once('/') {
            Some((addr, len)) => (addr, len.parse::<u8>().ok()?),
            None => (net, 32),
        };
        let addr: Ipv4Addr = addr.parse().ok()?;
        let low = u32::from(addr);
        let host = u32::MAX.checked_shr(u32::from(len)).unwrap_or(0);
        let sound = (24..=32).contains(&len) && low & host == 0;
        let touches = low <= LINK_LOCAL.1 && (low | host) >= LINK_LOCAL.0;
        (sound && !touches).then_some(Self { addr, len, port })
    }
}

impl Allow {
    /// Whether the endpoint lies in (or holds) `addr/len`.
    fn overlaps(&self, (addr, len): (Ipv4Addr, u8)) -> bool {
        let range = |addr: Ipv4Addr, len: u8| {
            let host = u32::MAX.checked_shr(u32::from(len)).unwrap_or(0);
            (u32::from(addr) & !host, u32::from(addr) | host)
        };
        let (mine, theirs) = (range(self.addr, self.len), range(addr, len));
        mine.0 <= theirs.1 && theirs.0 <= mine.1
    }
}

impl fmt::Display for Allow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", prefix(self.addr, self.len), self.port)
    }
}

/// `addr`, or `addr/len` below a /32: how nft prints an address.
fn prefix(addr: Ipv4Addr, len: u8) -> String {
    match len {
        32 => addr.to_string(),
        _ => format!("{addr}/{len}"),
    }
}

/// The checked egress settings: the endpoints and the `nft` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    allow: Vec<Allow>,
    nft: String,
}

impl Egress {
    /// The endpoints, sorted, each once, and the absolute path of `nft`.
    pub fn new(mut allow: Vec<Allow>, nft: &str) -> Self {
        allow.sort_by_key(|a| (u32::from(a.addr), a.len, a.port));
        allow.dedup();
        Self {
            allow,
            nft: nft.to_owned(),
        }
    }

    /// The endpoints, as the flag spelled them.
    pub fn allow(&self) -> &[Allow] {
        &self.allow
    }

    /// The `nft` program.
    pub fn nft(&self) -> &str {
        &self.nft
    }
}

/// Why the egress check failed. The text names what is wrong and never holds the output of
/// `podman` or `nft`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    /// `network inspect` failed.
    #[error("egress: cannot read network {name}: {why}")]
    Network {
        /// The network.
        name: String,
        /// The runner's reason.
        why: String,
    },
    /// The network is not a bridge, has DNS or IPv6, has not exactly one IPv4 subnet, or
    /// names no interface.
    #[error(
        "egress: network {name} must be a bridge with DNS disabled, no IPv6 and exactly one IPv4 subnet"
    )]
    NetworkShape {
        /// The network.
        name: String,
    },
    /// `br_netfilter` is not active: bridged traffic would not reach the firewall hooks.
    #[error("egress: br_netfilter is not active (bridge-nf-call-iptables is not 1)")]
    BrNetfilter,
    /// An endpoint lies in the network's own subnet.
    #[error("egress: a --container-egress-allow endpoint lies in the network's subnet")]
    AllowInSubnet,
    /// `nft` failed (the table is not loaded, or nft cannot run).
    #[error("egress: cannot list table inet js_egress: {0}")]
    Nft(String),
    /// What `nft` printed is not the JSON list of a table.
    #[error("egress: unreadable nft output")]
    Unreadable,
    /// The table is not in the listing.
    #[error("egress: table inet js_egress is not loaded")]
    NoTable,
    /// A chain is missing, is not a filter chain on its hook, or does not hold exactly
    /// the rules for the network's subnet and the endpoints.
    #[error(
        "egress: chain {chain} of inet js_egress is not what --container-egress-allow requires"
    )]
    Chain {
        /// `forward` or `input`.
        chain: &'static str,
    },
    /// A rule sits in a chain that is neither `forward` nor `input`.
    #[error("egress: inet js_egress holds a rule in a chain the agent does not expect")]
    Extra,
}

/// What the check needs to know of the network.
struct Net {
    interface: String,
    subnet: (Ipv4Addr, u8),
}

/// The network when it is as an egress network must be.
fn network(json: &str) -> Option<Net> {
    let value: Value = serde_json::from_str(json).ok()?;
    let net = value.as_array().filter(|a| a.len() == 1)?.first()?;
    // Fail closed: each field must be there and as required.
    let is = |key: &str, want: Value| net.get(key) == Some(&want);
    (is("driver", "bridge".into())
        && is("dns_enabled", false.into())
        && is("ipv6_enabled", false.into()))
    .then_some(())?;
    let interface = net.get("network_interface")?.as_str()?;
    let sound = (1..=15).contains(&interface.len())
        && interface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    let subnets = net.get("subnets")?.as_array().filter(|s| s.len() == 1)?;
    let (addr, len) = subnets[0].get("subnet")?.as_str()?.split_once('/')?;
    let (addr, len): (Ipv4Addr, u8) = (addr.parse().ok()?, len.parse().ok()?);
    (sound && len <= 30).then(|| Net {
        interface: interface.to_owned(),
        subnet: (addr, len),
    })
}

/// Checks the network, `br_netfilter` and the firewall. `bin` is the container runtime
/// (Podman) with its environment; `proc` reads a file of `/proc`.
///
/// # Errors
///
/// What is wrong; see [`EgressError`].
pub fn check(
    runner: &dyn CommandRunner,
    (bin, env): (&str, &[(&str, &str)]),
    name: &str,
    egress: &Egress,
    proc: &dyn Fn(&str) -> Option<String>,
) -> Result<(), EgressError> {
    let json = runner
        .run_in(bin, &["network", "inspect", "--", name], env)
        .map_err(|why| EgressError::Network {
            name: name.to_owned(),
            why,
        })?;
    let net = network(&json).ok_or_else(|| EgressError::NetworkShape {
        name: name.to_owned(),
    })?;
    if egress.allow().iter().any(|a| a.overlaps(net.subnet)) {
        return Err(EgressError::AllowInSubnet);
    }
    if proc(BR_NETFILTER).is_none_or(|text| text.trim() != "1") {
        return Err(EgressError::BrNetfilter);
    }
    let args = ["-j", "list", "table", "inet", TABLE];
    let listing = runner
        .run_in(egress.nft(), &args, &[])
        .map_err(EgressError::Nft)?;
    compare(&listing, &net, egress.allow())
}

/// The listing of `nft -j list table inet js_egress` against the rules for `net` and
/// `allow`.
fn compare(listing: &str, net: &Net, allow: &[Allow]) -> Result<(), EgressError> {
    let value: Value = serde_json::from_str(listing).map_err(|_| EgressError::Unreadable)?;
    let items = value
        .get("nftables")
        .and_then(Value::as_array)
        .ok_or(EgressError::Unreadable)?;
    let object = |key: &'static str| items.iter().filter_map(move |item| item.get(key));
    let is_ours = object("table")
        .any(|t| t.get("family") == Some(&"inet".into()) && t.get("name") == Some(&TABLE.into()));
    if !is_ours {
        return Err(EgressError::NoTable);
    }
    let rules: Vec<(&str, String)> = object("rule")
        .map(|rule| {
            let chain = rule.get("chain").and_then(Value::as_str).unwrap_or("");
            (chain, canonical(rule.get("expr")))
        })
        .collect();
    if rules
        .iter()
        .any(|(chain, _)| !matches!(*chain, "forward" | "input"))
    {
        return Err(EgressError::Extra);
    }
    let (i, (addr, len)) = (&net.interface, net.subnet);
    let sub = prefix(addr, len);
    let mut accepts: Vec<String> = allow
        .iter()
        .map(|a| {
            let to = prefix(a.addr, a.len);
            format!(
                "iif {i} saddr {sub} daddr {to} dport {} ct new accept",
                a.port
            )
        })
        .collect();
    accepts.sort();
    let v6 = format!("iif {i} nfproto ipv6 drop");
    let metadata = format!("iif {i} daddr 169.254.0.0/16 drop");
    let established = format!("iif {i} ct established,related accept");
    let last = format!("iif {i} drop");
    let chains = [
        ("forward", vec![v6.clone(), metadata, established.clone()]),
        ("input", vec![v6, established]),
    ];
    for (chain, leading) in chains {
        let hooked = object("chain").any(|c| {
            let text = |key: &str| c.get(key).and_then(Value::as_str);
            text("name") == Some(chain)
                && text("hook") == Some(chain)
                && text("type") == Some("filter")
                && c.get("prio").and_then(Value::as_i64) == Some(PRIORITY)
        });
        let have: Vec<&str> = rules
            .iter()
            .filter(|(c, _)| *c == chain)
            .map(|(_, rule)| rule.as_str())
            .collect();
        let n = leading.len();
        let fits = have.len() == n + accepts.len() + 1
            && have[..n] == leading[..]
            && have[have.len() - 1] == last;
        let mut middle: Vec<&str> = have
            .get(n..have.len().saturating_sub(1))
            .unwrap_or(&[])
            .to_vec();
        middle.sort_unstable();
        let wanted: Vec<&str> = accepts.iter().map(String::as_str).collect();
        if !(hooked && fits && middle == wanted) {
            return Err(EgressError::Chain { chain });
        }
    }
    Ok(())
}

/// A rule as one string of the words the check compares, `?` for any expression it does
/// not know (which then equals nothing expected). Counters are not part of a rule.
fn canonical(expr: Option<&Value>) -> String {
    let words: Vec<String> = expr
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(word).collect())
        .unwrap_or_default();
    words.join(" ")
}

/// The words of one expression of a rule.
fn word(expr: &Value) -> Option<String> {
    let (kind, body) = expr.as_object()?.iter().next()?;
    match kind.as_str() {
        "counter" => None,
        "accept" => Some("accept".to_owned()),
        "drop" => Some("drop".to_owned()),
        "match" => Some(matched(body).unwrap_or_else(|| "?".to_owned())),
        _ => Some("?".to_owned()),
    }
}

/// `iif br0`, `nfproto ipv6`, `saddr 192.0.2.0/24`, `daddr ..`, `dport 443` or `ct new`
/// for a match, when it is one of those with the operator `==` (or `in`, which nft uses
/// for flags).
fn matched(body: &Value) -> Option<String> {
    matches!(body.get("op")?.as_str()?, "==" | "in").then_some(())?;
    let (left, right) = (body.get("left")?, body.get("right")?);
    if let Some(ct) = left.get("ct") {
        (ct.get("key")?.as_str()? == "state").then_some(())?;
        let mut states: Vec<&str> = match right {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => return None,
        };
        states.sort_unstable();
        return Some(format!("ct {}", states.join(",")));
    }
    if let Some(meta) = left.get("meta") {
        let (key, value) = (meta.get("key")?.as_str()?, right.as_str()?);
        return match key {
            "iifname" => Some(format!("iif {value}")),
            "nfproto" => Some(format!("nfproto {value}")),
            _ => None,
        };
    }
    let load = left.get("payload")?;
    let (proto, field) = (
        load.get("protocol")?.as_str()?,
        load.get("field")?.as_str()?,
    );
    match (proto, field) {
        ("ip", "saddr") => Some(format!("saddr {}", address(right)?)),
        ("ip", "daddr") => Some(format!("daddr {}", address(right)?)),
        ("tcp", "dport") => Some(format!("dport {}", right.as_u64()?)),
        _ => None,
    }
}

/// An address or prefix as nft's JSON gives it, in the form of [`prefix`].
fn address(right: &Value) -> Option<String> {
    match right {
        Value::String(addr) => Some(addr.parse::<Ipv4Addr>().ok()?.to_string()),
        Value::Object(_) => {
            let body = right.get("prefix")?;
            let addr: Ipv4Addr = body.get("addr")?.as_str()?.parse().ok()?;
            Some(prefix(addr, u8::try_from(body.get("len")?.as_u64()?).ok()?))
        }
        _ => None,
    }
}
