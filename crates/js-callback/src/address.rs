//! The address rule of a callback (`docs/design/job-retry-and-completion-callback.md`, section
//! 5.3): with no allow-list, the server must never be turned against itself or its cluster, so
//! every address a try would connect to is judged here first.
//!
//! - **Admitted:** global unicast addresses, and the private ranges `10.0.0.0/8`,
//!   `172.16.0.0/12`, `192.168.0.0/16` and `fc00::/7`.
//! - **Denied within them:** `168.63.129.16` and `fd00:ec2::254`, where providers serve metadata
//!   outside the link-local range, and the cluster's own addresses.
//! - **Denied by default:** everything else: loopback, unspecified, `0.0.0.0/8`, link-local
//!   (`169.254.169.254` among it), carrier-grade NAT (`100.100.100.200` among it), multicast,
//!   broadcast, the deprecated site-local `fec0::/10`, and the rest of the IANA special-purpose
//!   ranges.
//! - **An IPv6 address that embeds an IPv4 one** is judged by that IPv4 address alone:
//!   IPv4-mapped, IPv4-compatible, NAT64 (`64:ff9b::/96`, and `64:ff9b:1::/48` read at each
//!   prefix length a translator may use within it), 6to4 and Teredo (its server and its
//!   client). Where it embeds more than one, each must be admitted.
//! - **A numeric host** (`2130706433`, `0x7f.1`, `017700000001`) is read as the address it names
//!   ([`literal`]), as the C library's address parser reads it, so it never reaches a resolver.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, PoisonError, RwLock};

/// A range of IPv4 addresses: its first address and its prefix length (1 to 32).
type Range4 = (Ipv4Addr, u32);

/// A range of IPv6 addresses: its first address and its prefix length (1 to 128).
type Range6 = (Ipv6Addr, u32);

/// The IPv4 addresses denied. Every address outside them is global unicast or private, and is
/// admitted.
const DENIED_V4: [Range4; 16] = [
    // "This network", the unspecified address among it.
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    // Carrier-grade NAT, 100.100.100.200 among it.
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    // Loopback.
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    // A provider's metadata and DNS address, which looks global.
    (Ipv4Addr::new(168, 63, 129, 16), 32),
    // Link-local, 169.254.169.254 among it.
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    // IETF protocol assignments.
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    // Documentation (TEST-NET-1).
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    // AS112.
    (Ipv4Addr::new(192, 31, 196, 0), 24),
    // AMT.
    (Ipv4Addr::new(192, 52, 193, 0), 24),
    // The deprecated 6to4 relay anycast.
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    // AS112 direct delegation.
    (Ipv4Addr::new(192, 175, 48, 0), 24),
    // Benchmarking.
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    // Documentation (TEST-NET-2).
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    // Documentation (TEST-NET-3).
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    // Multicast.
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    // Reserved, the broadcast address among it.
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The IPv6 addresses that may be admitted: global unicast and unique local.
const ADMITTED_V6: [Range6; 2] = [
    (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
];

/// The IPv6 addresses denied within [`ADMITTED_V6`].
const DENIED_V6: [Range6; 5] = [
    // IETF protocol assignments (Teredo, among them, is judged by its IPv4 addresses first).
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    // Documentation.
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    // AS112 direct delegation.
    (Ipv6Addr::new(0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0), 48),
    // Documentation.
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    // A provider's metadata address, inside fc00::/7.
    (Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254), 128),
];

/// The IPv6 ranges whose last 32 bits are an IPv4 address: IPv4-mapped, IPv4-compatible and
/// the NAT64 well-known prefix.
const LAST_32_BITS: [Range6; 3] = [
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
];

/// 6to4: the IPv4 address follows the first 16 bits.
const SIX_TO_FOUR: Range6 = (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16);

/// Teredo: the server's IPv4 address follows the first 32 bits, and the client's, with every
/// bit inverted, is the last 32.
const TEREDO: Range6 = (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32);

/// The NAT64 prefix for local use, within which a translator may use a prefix of 48, 56, 64 or
/// 96 bits (RFC 6052, section 2.2).
const NAT64_LOCAL: Range6 = (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48);

/// The addresses a callback may be sent to, as section 5.3 of the design draws them, and the
/// cluster's own addresses, which it never may.
///
/// A clone shares the set of cluster addresses, so an address the server learns after it built
/// the sender ([`AddressRule::add_cluster_address`]) is denied from then on.
#[derive(Debug, Clone)]
pub struct AddressRule {
    /// The cluster's own addresses.
    cluster: Arc<RwLock<BTreeSet<IpAddr>>>,
    /// Whether a loopback address is admitted (tests only).
    loopback: bool,
}

impl AddressRule {
    /// The rule of section 5.3, with `cluster` as the cluster's own addresses: this instance's
    /// listen addresses, the store host's resolved addresses and the peer address each host's
    /// heartbeat arrives from.
    /// An IPv4-mapped address among them is kept as the IPv4 address it maps.
    pub fn new(cluster: impl IntoIterator<Item = IpAddr>) -> AddressRule {
        AddressRule {
            cluster: Arc::new(RwLock::new(
                cluster.into_iter().map(|one| one.to_canonical()).collect(),
            )),
            loopback: false,
        }
    }

    /// This rule, but admitting the loopback addresses `127.0.0.0/8` and `::1` as written,
    /// whatever else it says of them: the rule a test's sender uses to reach a receiver that
    /// listens on loopback. The server never uses it.
    #[must_use]
    pub fn admit_loopback(self) -> AddressRule {
        AddressRule {
            loopback: true,
            ..self
        }
    }

    /// Denies `address` from now on, as one of the cluster's own, in this rule and in every
    /// clone of it. An IPv4-mapped address (a peer seen on a dual-stack listener) is kept as
    /// the IPv4 address it maps, so either spelling is denied.
    pub fn add_cluster_address(&self, address: IpAddr) {
        let mut cluster = self.cluster.write().unwrap_or_else(PoisonError::into_inner);
        cluster.insert(address.to_canonical());
    }

    /// Whether a callback may be sent to `address`.
    #[must_use]
    pub fn admits(&self, address: IpAddr) -> bool {
        if self.loopback && address.is_loopback() {
            return true;
        }
        let cluster = self.cluster.read().unwrap_or_else(PoisonError::into_inner);
        !cluster.contains(&address)
            && judged_by(address)
                .into_iter()
                .all(|one| !cluster.contains(&one) && in_admitted_range(one))
    }
}

/// Whether `address` lies in `range`.
fn within4(address: Ipv4Addr, (first, len): Range4) -> bool {
    (u32::from(address) ^ u32::from(first)) >> (32 - len) == 0
}

/// Whether `address` lies in `range`.
fn within6(address: Ipv6Addr, (first, len): Range6) -> bool {
    (u128::from(address) ^ u128::from(first)) >> (128 - len) == 0
}

/// The addresses `address` is judged by: each IPv4 address an IPv6 address embeds, or else
/// `address` itself.
fn judged_by(address: IpAddr) -> Vec<IpAddr> {
    let IpAddr::V6(v6) = address else {
        return vec![address];
    };
    let bits = u128::from(v6);
    // The low 32 bits of `value`, as an IPv4 address.
    let v4 = |value: u128| IpAddr::V4(Ipv4Addr::from(value as u32));
    if LAST_32_BITS.iter().any(|range| within6(v6, *range)) {
        vec![v4(bits)]
    } else if within6(v6, SIX_TO_FOUR) {
        vec![v4(bits >> 80)]
    } else if within6(v6, TEREDO) {
        vec![v4(bits >> 64), v4(!bits)]
    } else if within6(v6, NAT64_LOCAL) {
        // RFC 6052 leaves bits 64 to 71 empty; without them, a prefix of n bits puts the IPv4
        // address in the 32 bits after the first n of the 120 left.
        let squeezed = ((bits >> 64) << 56) | (bits & ((1 << 56) - 1));
        vec![
            v4(squeezed >> 40),
            v4(squeezed >> 32),
            v4(squeezed >> 24),
            v4(bits),
        ]
    } else {
        vec![address]
    }
}

/// Whether `address`, which embeds no other, is global unicast or private, and none of the
/// addresses denied within those.
fn in_admitted_range(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !DENIED_V4.iter().any(|range| within4(v4, *range)),
        IpAddr::V6(v6) => {
            ADMITTED_V6.iter().any(|range| within6(v6, *range))
                && !DENIED_V6.iter().any(|range| within6(v6, *range))
        }
    }
}

/// The address `host` names when it is a literal rather than a name: an IPv6 address, in
/// brackets or not, or an IPv4 address in any form the C library's `inet_aton` reads (one to
/// four parts, each decimal, octal after a leading `0`, or hexadecimal after `0x`, the last
/// filling the bytes left).
pub(crate) fn literal(host: &str) -> Option<IpAddr> {
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let v6 = bare.parse().ok().map(IpAddr::V6);
    v6.or_else(|| numeric_v4(bare).map(IpAddr::V4))
}

/// The IPv4 address `host` names in `inet_aton`'s forms, if it is one.
fn numeric_v4(host: &str) -> Option<Ipv4Addr> {
    let parts: Option<Vec<u64>> = host.split('.').map(number).collect();
    let parts = parts.filter(|parts| parts.len() <= 4)?;
    // `split` gives at least one part, so `last` is always there.
    let last = parts.last().copied().unwrap_or_default();
    let leading = &parts[..parts.len() - 1];
    let room = 8 * (4 - leading.len());
    let fits = leading.iter().all(|part| *part <= 0xff) && last >> room == 0;
    let high = leading
        .iter()
        .fold(0_u64, |value, part| (value << 8) | *part);
    let value = (high << room) | last;
    fits.then_some(Ipv4Addr::from(value as u32))
}

/// The value of one part of a numeric IPv4 host: hexadecimal after `0x`, octal after a
/// leading `0`, else decimal.
fn number(part: &str) -> Option<u64> {
    let lower = part.to_ascii_lowercase();
    let (digits, radix) = match lower.strip_prefix("0x") {
        Some(hex) => (hex, 16),
        None if lower.len() > 1 && lower.starts_with('0') => (&lower[1..], 8),
        None => (lower.as_str(), 10),
    };
    let plain = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit());
    plain
        .then(|| u64::from_str_radix(digits, radix).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` as an address.
    fn ip(text: &str) -> IpAddr {
        text.parse().expect(text)
    }

    /// Each of `texts` with whether `rule` admits it.
    fn verdicts<'a>(rule: &AddressRule, texts: &[&'a str]) -> Vec<(&'a str, bool)> {
        texts
            .iter()
            .map(|text| (*text, rule.admits(ip(text))))
            .collect()
    }

    /// Asserts, under the production rule, a verdict for each class section 5.3 names: every
    /// denied IPv4 range at its first or last address, `168.63.129.16`, every denied IPv6 range
    /// and default denial, `fd00:ec2::254`, each embedded-IPv4 form carrying a denied address
    /// and the same form carrying a public one, both Teredo halves, `64:ff9b:1::/48` read at
    /// each prefix length, and each cluster address (IPv4, IPv4-mapped and IPv6); and that a
    /// public and a private address of each family are admitted, with the edges of the ranges
    /// next to them. Catches a range admitted that is denied, a private range refused, a
    /// prefix length off by one, an embedded address judged as the IPv6 one around it, and a
    /// cluster address let through in another spelling.
    #[test]
    fn the_address_rule_refuses_each_denied_range() {
        let cluster = [
            ip("10.0.0.5"),
            ip("::ffff:10.0.0.7"),
            ip("2001:4860:4860::8844"),
        ];
        let rule = AddressRule::new(cluster);
        let denied = [
            "0.0.0.0",
            "0.255.255.255",
            "100.64.0.0",
            "100.100.100.200",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.255",
            "168.63.129.16",
            "169.254.169.254",
            "192.0.0.9",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "198.18.0.0",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.7",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "100::1",
            "1fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "4000::1",
            "fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe00::1",
            "2001:2::1",
            "2001:1ff::1",
            "2001:db8::1",
            "2620:4f:8000::1",
            "3fff::1",
            "3fff:fff::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:a9fe:a9fe::1",
            // Teredo: the client is 127.0.0.1, then the server is.
            "2001:0:808:808::80ff:fffe",
            "2001:0:7f00:1::f7f7:f7f7",
            // 127.0.0.0 at a 48-bit prefix, 8.8.8.8 at a 96-bit one.
            "64:ff9b:1:7f00:0:1:808:808",
            // The cluster's own, written as given and as IPv4-mapped, either way round.
            "10.0.0.5",
            "::ffff:10.0.0.5",
            "10.0.0.7",
            "::ffff:10.0.0.7",
            "2001:4860:4860::8844",
        ];
        assert_eq!(verdicts(&rule, &denied), denied.map(|text| (text, false)));
        let admitted = [
            "8.8.8.8",
            "10.0.0.6",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.63.255.255",
            "100.128.0.0",
            "168.63.129.17",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2001:4860:4860::8888",
            "2001:200::1",
            "2620:4f:8001::1",
            "3fff:1000::1",
            "fc00::1",
            "fd12:3456::1",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fd00:ec2::253",
            "::ffff:8.8.8.8",
            "::8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001:0:808:808::f7f7:f7f7",
            "64:ff9b:1:808:808:808:808:808",
        ];
        assert_eq!(
            verdicts(&rule, &admitted),
            admitted.map(|text| (text, true))
        );
    }

    /// Asserts that an address added to the cluster after a rule was cloned is denied by the
    /// clone, as the sender holds a clone of the server's rule, also when it was learned in its
    /// IPv4-mapped form. Catches a clone that copies the set, which would let a host's newly
    /// learned address through, and a mapped peer address that leaves its IPv4 form admitted.
    #[test]
    fn a_cluster_address_learned_later_is_denied_by_every_clone() {
        let rule = AddressRule::new([]);
        let held = rule.clone();
        assert_eq!(verdicts(&held, &["10.1.2.3"]), [("10.1.2.3", true)]);
        rule.add_cluster_address(ip("10.1.2.3"));
        rule.add_cluster_address(ip("::ffff:10.1.2.5"));
        assert_eq!(
            verdicts(&held, &["10.1.2.3", "10.1.2.4", "10.1.2.5"]),
            [("10.1.2.3", false), ("10.1.2.4", true), ("10.1.2.5", false)]
        );
    }

    /// Asserts that the tests' rule admits `127.0.0.1`, `127.9.9.9` and `::1`, and still
    /// denies the metadata address, an IPv4-mapped loopback address, and a cluster address;
    /// and that the production rule denies loopback. Catches a test rule that admits
    /// everything, and a production rule that admits loopback.
    #[test]
    fn the_test_rule_admits_loopback_and_nothing_more() {
        let rule = AddressRule::new([ip("10.0.0.5")]).admit_loopback();
        let texts = [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "169.254.169.254",
            "::ffff:127.0.0.1",
            "10.0.0.5",
        ];
        assert_eq!(
            verdicts(&rule, &texts),
            [
                ("127.0.0.1", true),
                ("127.9.9.9", true),
                ("::1", true),
                ("169.254.169.254", false),
                ("::ffff:127.0.0.1", false),
                ("10.0.0.5", false),
            ]
        );
        assert_eq!(
            verdicts(&AddressRule::new([]), &["127.0.0.1"]),
            [("127.0.0.1", false)]
        );
    }

    /// Asserts the address each numeric host form names, as `inet_aton` reads it: one number,
    /// two, three and four parts, hexadecimal in either case, octal, a zero, and an IPv6
    /// literal with and without its brackets; and that a name, five parts, a part too large for its place, a value past 32
    /// bits, an empty part, a bare `0x`, a sign, an octal 9 and an unparsable IPv6 literal are
    /// not literals (so the resolver is asked, and its answer judged). Catches `2130706433`
    /// or `0x7f.1` passed to a resolver as a name, a part read in the wrong base, and a
    /// form read as an address it does not name.
    #[test]
    fn numeric_host_forms_are_read_as_the_address_they_name() {
        let named = [
            ("2130706433", "127.0.0.1"),
            ("0x7f.1", "127.0.0.1"),
            ("0X7F.0.1", "127.0.0.1"),
            ("127.0.0.1", "127.0.0.1"),
            ("017700000001", "127.0.0.1"),
            ("0251.0376.0251.0376", "169.254.169.254"),
            ("0xa9fea9fe", "169.254.169.254"),
            ("169.254.43518", "169.254.169.254"),
            ("0", "0.0.0.0"),
            ("[::1]", "::1"),
            ("::1", "::1"),
            ("[::ffff:7f00:1]", "::ffff:127.0.0.1"),
        ];
        for (host, address) in named {
            assert_eq!(literal(host), Some(ip(address)), "{host}");
        }
        let names = [
            "hooks.example",
            "deadbeef",
            "1.2.3.4.5",
            "256.1.1.1",
            "1.2.3.256",
            "1.2.65536",
            "4294967296",
            "1..2",
            "1.2.3.4.",
            "0x",
            "+1",
            "09",
            "[zz]",
            "[::1",
        ];
        for host in names {
            assert_eq!(literal(host), None, "{host}");
        }
    }
}
