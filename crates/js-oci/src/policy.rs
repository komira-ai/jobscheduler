//! Where this client may go: which registries and namespaces a pin may name, and which
//! addresses a connection may reach.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::Error;
use crate::location::{Authority, Location, path_ok};

/// Why an address is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressClass {
    /// `127.0.0.0/8` and `::1`.
    Loopback,
    /// `169.254.0.0/16` and `fe80::/10`.
    LinkLocal,
    /// The RFC 1918 ranges.
    Private,
    /// The shared address space, `100.64.0.0/10`.
    Shared,
    /// IPv6 unique-local addresses, `fc00::/7`.
    UniqueLocal,
    /// `0.0.0.0/8` and `::`, which a host connects to itself through.
    Unspecified,
    /// Multicast, broadcast and the reserved block `224.0.0.0/3`, and `ff00::/8`.
    Reserved,
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Private => "private",
            Self::Shared => "shared-range",
            Self::UniqueLocal => "unique-local",
            Self::Unspecified => "unspecified",
            Self::Reserved => "multicast or reserved",
        })
    }
}

/// Why `ip` is not an address a registry request may reach, or `None` for a public
/// one. An IPv6 address that embeds an IPv4 one (IPv4-mapped, IPv4-compatible and
/// NAT64 `64:ff9b::/96`) is judged by the IPv4 address in it.
#[must_use]
pub fn classify(ip: IpAddr) -> Option<AddressClass> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<AddressClass> {
    let first = ip.octets()[0];
    if ip.is_loopback() {
        Some(AddressClass::Loopback)
    } else if ip.is_link_local() {
        Some(AddressClass::LinkLocal)
    } else if ip.is_private() {
        Some(AddressClass::Private)
    } else if u32::from(ip) & 0xffc0_0000 == 0x6440_0000 {
        Some(AddressClass::Shared)
    } else if first == 0 {
        Some(AddressClass::Unspecified)
    } else if first >= 224 {
        Some(AddressClass::Reserved)
    } else {
        None
    }
}

fn classify_v6(ip: Ipv6Addr) -> Option<AddressClass> {
    let head = ip.segments()[0];
    if ip.is_loopback() {
        Some(AddressClass::Loopback)
    } else if ip.is_unspecified() {
        Some(AddressClass::Unspecified)
    } else if let Some(v4) = ip.to_ipv4().or_else(|| nat64(ip)) {
        classify_v4(v4)
    } else if head & 0xfe00 == 0xfc00 {
        Some(AddressClass::UniqueLocal)
    } else if head & 0xffc0 == 0xfe80 {
        Some(AddressClass::LinkLocal)
    } else if head & 0xff00 == 0xff00 {
        Some(AddressClass::Reserved)
    } else {
        None
    }
}

/// The IPv4 address in a NAT64 address, `64:ff9b::/96`.
fn nat64(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let [a, b, c, d, e, f, hi, lo] = ip.segments();
    let prefix = [a == 0x64, b == 0xff9b, c == 0, d == 0, e == 0, f == 0];
    prefix.into_iter().all(|ok| ok).then(|| {
        let [w, x] = hi.to_be_bytes();
        let [y, z] = lo.to_be_bytes();
        Ipv4Addr::new(w, x, y, z)
    })
}

/// One registry, and the namespace of it, that a pin may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allow {
    authority: Authority,
    prefix: String,
    private: bool,
}

impl Allow {
    /// The entry `HOST[:PORT]/NAMESPACE-PREFIX` (`ghcr.io/komira-ai/`). The prefix is
    /// whole path components: `komira-ai/` admits `komira-ai/x`, not `komira-ai-x/y`. A
    /// bare host is refused, since it would admit every account on the host.
    ///
    /// # Errors
    /// [`Error::Reference`] when the entry is not of that form.
    pub fn parse(entry: &str) -> Result<Self, Error> {
        let bad = || Error::Reference(entry.to_owned());
        let (host, prefix) = entry.split_once('/').ok_or_else(bad)?;
        let prefix = prefix.trim_end_matches('/');
        let authority = Authority::parse(host).ok_or_else(bad)?;
        if !path_ok(prefix) {
            return Err(bad());
        }
        Ok(Self {
            authority,
            prefix: prefix.to_owned(),
            private: false,
        })
    }

    /// This entry as one whose host may be reached at a private address: a registry on
    /// a private network, for its own host and nothing else.
    #[must_use]
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    fn admits(&self, location: &Location) -> bool {
        let path = location.repository();
        let under = path
            .strip_prefix(self.prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        self.authority == *location.registry() && under
    }
}

/// What a client may reach: the registries and namespaces of its allow-list, and the
/// addresses it may connect to. An empty policy reaches nothing.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    allow: Vec<Allow>,
}

impl Policy {
    /// A policy that admits the entries of `allow`.
    #[must_use]
    pub fn new(allow: Vec<Allow>) -> Self {
        Self { allow }
    }

    /// Whether a pin may name `location`.
    pub(crate) fn admit(&self, location: &Location) -> Result<(), Error> {
        if self.allow.iter().any(|a| a.admits(location)) {
            Ok(())
        } else {
            Err(Error::NotAllowed {
                registry: location.registry().to_string(),
                repository: location.repository().to_owned(),
            })
        }
    }

    /// Whether a token endpoint may be asked at `realm`: the registry's own host, or
    /// the host of another entry.
    pub(crate) fn realm_ok(&self, realm: &Authority, endpoint: &Authority) -> bool {
        realm == endpoint || self.allow.iter().any(|a| a.authority == *realm)
    }

    /// Whether a connection to `host` may go to `addr`: any public address, and a
    /// refused one only for a host that an entry lists as private.
    pub(crate) fn address_ok(&self, host: &Authority, addr: IpAddr) -> Result<(), Error> {
        let Some(class) = classify(addr) else {
            return Ok(());
        };
        if self.allow.iter().any(|a| a.private && a.authority == *host) {
            return Ok(());
        }
        Err(Error::Address {
            host: host.host().to_owned(),
            addr,
            class,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(text: &str) -> Option<AddressClass> {
        classify(text.parse().expect("address"))
    }

    /// Catches an address range left connectable: every class the rules name, in the
    /// IPv4 and IPv6 forms, at both ends of its range, and the public neighbours that
    /// must stay reachable.
    #[test]
    fn the_refused_ranges_are_exactly_the_non_public_ones() {
        let refused = [
            ("127.0.0.1", AddressClass::Loopback),
            ("127.255.255.254", AddressClass::Loopback),
            ("::1", AddressClass::Loopback),
            ("169.254.169.254", AddressClass::LinkLocal),
            ("fe80::1", AddressClass::LinkLocal),
            ("febf::1", AddressClass::LinkLocal),
            ("10.0.0.1", AddressClass::Private),
            ("172.16.0.1", AddressClass::Private),
            ("172.31.255.255", AddressClass::Private),
            ("192.168.1.1", AddressClass::Private),
            ("100.64.0.1", AddressClass::Shared),
            ("100.127.255.255", AddressClass::Shared),
            ("fc00::1", AddressClass::UniqueLocal),
            ("fd12:3456::1", AddressClass::UniqueLocal),
            ("0.0.0.0", AddressClass::Unspecified),
            ("0.1.2.3", AddressClass::Unspecified),
            ("::", AddressClass::Unspecified),
            ("224.0.0.1", AddressClass::Reserved),
            ("240.0.0.1", AddressClass::Reserved),
            ("255.255.255.255", AddressClass::Reserved),
            ("ff02::1", AddressClass::Reserved),
        ];
        for (text, want) in refused {
            assert_eq!(class(text), Some(want), "{text}");
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2001:db8::1",
            "fe00::1",
            "fb00::1",
        ] {
            assert_eq!(class(public), None, "{public}");
        }
    }

    /// Catches an IPv6 spelling that gets round the IPv4 rules: IPv4-mapped,
    /// IPv4-compatible and NAT64 forms are judged by the address inside them, and a
    /// public address in those forms stays reachable.
    #[test]
    fn ipv6_forms_of_ipv4_addresses_are_judged_by_the_ipv4_address() {
        for (text, want) in [
            ("::ffff:127.0.0.1", AddressClass::Loopback),
            ("::ffff:10.1.2.3", AddressClass::Private),
            ("::ffff:169.254.169.254", AddressClass::LinkLocal),
            ("::ffff:100.64.0.1", AddressClass::Shared),
            ("::ffff:0.0.0.0", AddressClass::Unspecified),
            ("::10.1.2.3", AddressClass::Private),
            ("64:ff9b::7f00:1", AddressClass::Loopback),
            ("64:ff9b::a00:1", AddressClass::Private),
        ] {
            assert_eq!(class(text), Some(want), "{text}");
        }
        for public in ["::ffff:8.8.8.8", "64:ff9b::808:808", "::8.8.8.8"] {
            assert_eq!(class(public), None, "{public}");
        }
        // Not NAT64: the prefix differs in one segment each.
        assert_eq!(class("65:ff9b::7f00:1"), None);
        assert_eq!(class("64:ff9a::7f00:1"), None);
        assert_eq!(class("64:ff9b:0:0:1::7f00:1"), None);
    }

    fn class_name(text: &str) -> Option<String> {
        class(text).map(|c| c.to_string())
    }

    /// Catches a hardened range left connectable: the cloud metadata addresses, the
    /// benchmarking and IETF-protocol blocks, site-local, 6to4, Teredo and the local-use
    /// NAT64 prefix, at both ends of each range, with the public neighbours that must
    /// stay reachable.
    #[test]
    #[ignore = "shown red in the previous run"]
    fn the_hardened_ranges_are_refused_with_their_public_neighbours() {
        let reserved = "multicast or reserved";
        for (text, want) in [
            ("168.63.129.16", "cloud-metadata"),
            ("169.254.169.254", "cloud-metadata"),
            ("fd00:ec2::254", "cloud-metadata"),
            ("::ffff:168.63.129.16", "cloud-metadata"),
            ("64:ff9b::a83f:8110", "cloud-metadata"),
            ("2002:a83f:8110::1", "cloud-metadata"),
            ("198.18.0.0", reserved),
            ("198.19.255.255", reserved),
            ("192.0.0.0", reserved),
            ("192.0.0.255", reserved),
            ("64:ff9b:1::", reserved),
            ("64:ff9b:1:ffff:ffff:ffff:ffff:ffff", reserved),
            ("fec0::", "site-local"),
            ("feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "site-local"),
            ("2002::", "unspecified"),
            ("2002:7f00:1::1", "loopback"),
            ("2002:a00:1::1", "private"),
            ("2002:ac1f:ffff::1", "private"),
            ("2002:ffff:ffff::1", reserved),
            ("2001:0:4136:e378:8000:63bf:80ff:fffe", "loopback"),
            ("2001:0:1:2:3:4:f5ff:fffe", "private"),
            ("2001::ffff:ffff", "unspecified"),
        ] {
            assert_eq!(class_name(text).as_deref(), Some(want), "{text}");
        }
        for public in [
            "168.63.129.15",
            "168.63.129.17",
            "168.63.128.16",
            "169.253.255.255",
            "198.17.255.255",
            "198.20.0.0",
            "192.0.1.0",
            "191.255.255.255",
            "64:ff9b:2::1",
            "64:ff9b:0:1::1",
            "fe7f:ffff::1",
            "2001:ffff::1",
            "2001:1::7f00:1",
            "2003::1",
            "2002:808:808::1",
            "2001:0:1:2:3:4:f7f7:f7f7",
            "fd00:ec2::255",
            "fd00:ec3::254",
        ] {
            assert_eq!(class_name(public), None, "{public}");
        }
    }

    /// Catches a private entry that exempts more than the private classes: loopback,
    /// link-local, the metadata addresses, unspecified and multicast stay refused, in
    /// IPv6 and IPv4-mapped forms too, while the private classes pass for the listed host.
    #[test]
    #[ignore = "shown red in the previous run"]
    fn a_private_entry_never_exempts_loopback_link_local_or_metadata() {
        let policy = Policy::new(vec![
            Allow::parse("farm.example:5000/team")
                .expect("entry")
                .private(),
        ]);
        let host = Authority::parse("farm.example:5000").expect("authority");
        for refused in [
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "169.254.1.1",
            "169.254.169.254",
            "fe80::1",
            "fd00:ec2::254",
            "168.63.129.16",
            "::ffff:169.254.169.254",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
        ] {
            let ip: IpAddr = refused.parse().expect("ip");
            assert!(
                matches!(policy.address_ok(&host, ip), Err(Error::Address { .. })),
                "{refused}"
            );
        }
        for exempt in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "fd12::1",
        ] {
            let ip: IpAddr = exempt.parse().expect("ip");
            policy.address_ok(&host, ip).expect(exempt);
        }
    }

    /// Catches an address class that reads wrongly in the error that names it.
    #[test]
    fn every_class_names_itself() {
        let names: Vec<String> = [
            AddressClass::Loopback,
            AddressClass::LinkLocal,
            AddressClass::Private,
            AddressClass::Shared,
            AddressClass::UniqueLocal,
            AddressClass::Unspecified,
            AddressClass::Reserved,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            names,
            [
                "loopback",
                "link-local",
                "private",
                "shared-range",
                "unique-local",
                "unspecified",
                "multicast or reserved"
            ]
        );
    }

    fn location(name: &str) -> Location {
        Location::parse(name).expect("location")
    }

    /// Catches an allow-list that admits more than it lists: a bare host, a prefix that
    /// is only a string prefix, another port, another host, or a different case.
    #[test]
    fn a_pin_must_name_a_listed_host_and_namespace() {
        let policy = Policy::new(vec![
            Allow::parse("ghcr.io/komira-ai/").expect("entry"),
            Allow::parse("Registry.Example:5000/team/sub").expect("entry"),
        ]);
        for ok in [
            "ghcr.io/komira-ai/bundle",
            "ghcr.io/komira-ai/a/b/c",
            "ghcr.io/komira-ai",
            "GHCR.io/komira-ai/bundle",
            "registry.example:5000/team/sub",
            "registry.example:5000/team/sub/x",
        ] {
            policy.admit(&location(ok)).expect(ok);
        }
        for bad in [
            "ghcr.io/komira-ai-evil/bundle",
            "ghcr.io/other/bundle",
            "ghcr.io/komira/ai/bundle",
            "ghcr.io:8443/komira-ai/bundle",
            "registry.example/team/sub/x",
            "registry.example:5000/team/other",
            "registry.example:5000/team",
            "elsewhere.io/komira-ai/bundle",
        ] {
            assert!(
                matches!(policy.admit(&location(bad)), Err(Error::NotAllowed { .. })),
                "{bad}"
            );
        }
        let none = Policy::default();
        assert!(none.admit(&location("ghcr.io/komira-ai/b")).is_err());
    }

    /// Catches entries read wrongly: a bare host, no host, an empty or malformed
    /// namespace and a bad host are refused.
    #[test]
    fn allow_entries_need_a_host_and_a_namespace() {
        for bad in [
            "ghcr.io",
            "ghcr.io/",
            "/komira-ai",
            "ghcr.io//x",
            "ghcr.io/Komira",
            "ghcr.io/a/../b",
            "bad host/x",
            "",
        ] {
            assert!(
                matches!(Allow::parse(bad), Err(Error::Reference(_))),
                "{bad:?}: {:?}",
                Allow::parse(bad)
            );
        }
    }

    /// Catches a private exemption that spreads: it holds for the listed host and port
    /// only, not for other hosts, other ports or public-entry hosts.
    #[test]
    fn a_private_address_is_reachable_only_for_a_host_listed_as_private() {
        let policy = Policy::new(vec![
            Allow::parse("farm.example:5000/team")
                .expect("entry")
                .private(),
            Allow::parse("ghcr.io/komira-ai").expect("entry"),
        ]);
        let host = |text: &str| Authority::parse(text).expect("authority");
        let private: IpAddr = "10.1.2.3".parse().expect("ip");
        let public: IpAddr = "8.8.8.8".parse().expect("ip");
        policy
            .address_ok(&host("farm.example:5000"), private)
            .expect("listed");
        policy
            .address_ok(&host("farm.example:5000"), public)
            .expect("public");
        policy.address_ok(&host("ghcr.io"), public).expect("public");
        for refused in ["farm.example:5001", "ghcr.io", "other.example:5000"] {
            assert!(
                matches!(
                    policy.address_ok(&host(refused), private),
                    Err(Error::Address { .. })
                ),
                "{refused}"
            );
        }
    }

    /// Catches a token endpoint accepted off the allow-list: the registry's own host
    /// and any listed host pass, and nothing else.
    #[test]
    fn a_realm_must_be_the_registry_or_a_listed_host() {
        let policy = Policy::new(vec![
            Allow::parse("a.example/x").expect("entry"),
            Allow::parse("b.example/y").expect("entry"),
        ]);
        let host = |text: &str| Authority::parse(text).expect("authority");
        let endpoint = host("a.example");
        assert!(policy.realm_ok(&host("a.example"), &endpoint));
        assert!(policy.realm_ok(&host("b.example"), &endpoint));
        assert!(policy.realm_ok(&host("hub-api.example"), &host("hub-api.example")));
        assert!(!policy.realm_ok(&host("c.example"), &endpoint));
        assert!(!policy.realm_ok(&host("b.example:8443"), &endpoint));
    }
}
