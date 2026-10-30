//! Where this client may go: which registries and namespaces a pin may name, and which
//! addresses a connection may reach.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::Error;
use crate::location::{Authority, Location, path_ok};
use crate::text::{clip, escape};

/// Why an address is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddressClass {
    /// `127.0.0.0/8` and `::1`.
    Loopback,
    /// `169.254.0.0/16` (except the metadata address) and `fe80::/10`.
    LinkLocal,
    /// The addresses clouds serve instance metadata and credentials on: `169.254.169.254`,
    /// `fd00:ec2::254`, Azure's `168.63.129.16` and Alibaba's `100.100.100.200` (which sits in
    /// the shared range and is judged as metadata first). Never exempt.
    Metadata,
    /// The RFC 1918 ranges.
    Private,
    /// The shared address space, `100.64.0.0/10`.
    Shared,
    /// IPv6 unique-local addresses, `fc00::/7`.
    UniqueLocal,
    /// The deprecated IPv6 site-local range, `fec0::/10`.
    SiteLocal,
    /// `0.0.0.0/8` and `::`, which a host connects to itself through.
    Unspecified,
    /// Multicast, broadcast and the reserved block `224.0.0.0/3`, `ff00::/8`, the
    /// benchmarking range `198.18.0.0/15`, the IETF protocol block `192.0.0.0/24` and
    /// the local-use NAT64 prefix `64:ff9b:1::/48`.
    Reserved,
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Metadata => "cloud-metadata",
            Self::Private => "private",
            Self::Shared => "shared-range",
            Self::UniqueLocal => "unique-local",
            Self::SiteLocal => "site-local",
            Self::Unspecified => "unspecified",
            Self::Reserved => "multicast or reserved",
        })
    }
}

/// Why `ip` is not an address a registry request may reach, or `None` for a public
/// one. An IPv6 address that embeds an IPv4 one is judged by the IPv4 address in it:
/// IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible, NAT64 (`64:ff9b::/96`), 6to4
/// (`2002::/16`, the address after the prefix) and Teredo (`2001::/32`, the client
/// address, which is stored inverted).
#[must_use]
pub fn classify(ip: IpAddr) -> Option<AddressClass> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<AddressClass> {
    let octets = ip.octets();
    if matches!(
        octets,
        [168, 63, 129, 16] | [169, 254, 169, 254] | [100, 100, 100, 200]
    ) {
        Some(AddressClass::Metadata)
    } else if ip.is_loopback() {
        Some(AddressClass::Loopback)
    } else if ip.is_link_local() {
        Some(AddressClass::LinkLocal)
    } else if ip.is_private() {
        Some(AddressClass::Private)
    } else if u32::from(ip) & 0xffc0_0000 == 0x6440_0000 {
        Some(AddressClass::Shared)
    } else if matches!(octets, [198, 18 | 19, ..] | [192, 0, 0, _]) {
        Some(AddressClass::Reserved)
    } else if octets[0] == 0 {
        Some(AddressClass::Unspecified)
    } else if octets[0] >= 224 {
        Some(AddressClass::Reserved)
    } else {
        None
    }
}

/// The metadata address of AWS on IPv6.
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

fn classify_v6(ip: Ipv6Addr) -> Option<AddressClass> {
    let head = ip.segments()[0];
    if ip.is_loopback() {
        Some(AddressClass::Loopback)
    } else if ip.is_unspecified() {
        Some(AddressClass::Unspecified)
    } else if let Some(v4) = embedded_v4(ip) {
        classify_v4(v4)
    } else if ip == METADATA_V6 {
        Some(AddressClass::Metadata)
    } else if matches!(ip.segments(), [0x64, 0xff9b, 1, ..]) {
        Some(AddressClass::Reserved)
    } else if head & 0xfe00 == 0xfc00 {
        Some(AddressClass::UniqueLocal)
    } else if head & 0xffc0 == 0xfe80 {
        Some(AddressClass::LinkLocal)
    } else if head & 0xffc0 == 0xfec0 {
        Some(AddressClass::SiteLocal)
    } else if head & 0xff00 == 0xff00 {
        Some(AddressClass::Reserved)
    } else {
        None
    }
}

/// The IPv4 address an IPv6 one carries: NAT64 `64:ff9b::/96`, 6to4 `2002::/16`, Teredo
/// `2001::/32` (the client, bit-inverted), or IPv4-mapped and IPv4-compatible.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    match ip.segments() {
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] | [0x2002, hi, lo, ..] => Some(v4(hi, lo)),
        [0x2001, 0, _, _, _, _, hi, lo] => Some(v4(!hi, !lo)),
        _ => ip.to_ipv4(),
    }
}

fn v4(hi: u16, lo: u16) -> Ipv4Addr {
    let [a, b] = hi.to_be_bytes();
    let [c, d] = lo.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// One registry, and the namespace of it, that a pin may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allow {
    authority: Authority,
    prefix: String,
    private: bool,
    loopback: bool,
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
            loopback: false,
        })
    }

    /// This entry as one whose host may be reached at a private address: a registry on
    /// a private network, for its own host and nothing else.
    ///
    /// The exemption covers the RFC 1918 ranges, the shared range `100.64.0.0/10` and
    /// IPv6 unique-local addresses, and only those. Loopback, link-local, the cloud
    /// metadata addresses (`169.254.169.254`, `fd00:ec2::254`, `168.63.129.16`, `100.100.100.200`),
    /// unspecified and multicast addresses are refused for a host listed as private,
    /// whatever it resolves to; see [`Allow::loopback`] for the one of those a local
    /// registry needs.
    ///
    /// The exemption is per connect target, not per registry: it applies to this host
    /// and port whoever asks to reach it. A public registry can therefore send a
    /// redirect, or point its token realm, at a host listed here, on port 443 or on the
    /// port of the registry that sent it, and the client will connect (a GET, with no
    /// credentials of the registry's). List a private host knowing that every registry
    /// the policy admits can name it that way.
    #[must_use]
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    /// This entry as one whose host may be reached at a loopback address (`127.0.0.0/8`,
    /// `::1`): a registry on this machine, for local development and tests, for its own
    /// host and nothing else. Never list a name an attacker can register or influence
    /// with it: a name that resolves to loopback would reach this machine's own
    /// services. It does not exempt link-local, metadata, unspecified or multicast
    /// addresses, and not the private ranges (that is [`Allow::private`]).
    /// The same per-target rule applies as for [`Allow::private`]: any admitted registry
    /// can redirect to this host.
    #[must_use]
    pub fn loopback(mut self) -> Self {
        self.loopback = true;
        self
    }

    /// Whether this entry's host may be reached at an address of `class`.
    fn exempts(&self, class: AddressClass) -> bool {
        match class {
            AddressClass::Private | AddressClass::Shared | AddressClass::UniqueLocal => {
                self.private
            }
            AddressClass::Loopback => self.loopback,
            _ => false,
        }
    }

    /// Whether a pin may name `location` under this entry: the same registry (host and
    /// port, the host in lower case) and a repository that is the entry's prefix or lies
    /// below it by whole path components.
    #[must_use]
    pub fn admits(&self, location: &Location) -> bool {
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
    realms: Vec<Authority>,
}

impl Policy {
    /// A policy that admits the entries of `allow`.
    #[must_use]
    pub fn new(allow: Vec<Allow>) -> Self {
        Self {
            allow,
            realms: Vec::new(),
        }
    }

    /// This policy with `host` (`host` or `host:port`) also accepted as a token
    /// endpoint. A pin cannot name it: it is a host to fetch tokens from, not a
    /// registry. A registry whose tokens come from another host needs this; Docker
    /// Hub's token service is `auth.docker.io`:
    ///
    /// ```
    /// use js_oci::{Allow, Policy};
    ///
    /// # fn main() -> Result<(), js_oci::Error> {
    /// let policy = Policy::new(vec![Allow::parse("docker.io/library")?])
    ///     .allow_realm("auth.docker.io")?;
    /// # let _ = policy;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// [`Error::Reference`] when `host` is not a host name or address with an optional
    /// port.
    pub fn allow_realm(mut self, host: &str) -> Result<Self, Error> {
        let authority = Authority::parse(host).ok_or_else(|| Error::Reference(clip(host)))?;
        self.realms.push(authority);
        Ok(self)
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

    /// Whether a token endpoint may be asked at `realm`: the registry's own host, the
    /// host of another entry, or a host added with [`Policy::allow_realm`].
    pub(crate) fn realm_ok(&self, realm: &Authority, endpoint: &Authority) -> bool {
        realm == endpoint
            || self.allow.iter().any(|a| a.authority == *realm)
            || self.realms.contains(realm)
    }

    /// Whether a connection to `host` may go to `addr`: any public address, and a
    /// refused one only when an entry for `host` exempts its class (see
    /// [`Allow::private`] and [`Allow::loopback`]).
    pub(crate) fn address_ok(&self, host: &Authority, addr: IpAddr) -> Result<(), Error> {
        let Some(class) = classify(addr) else {
            return Ok(());
        };
        if self
            .allow
            .iter()
            .any(|a| a.authority == *host && a.exempts(class))
        {
            return Ok(());
        }
        Err(Error::Address {
            host: escape(host.host()),
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
            ("169.254.0.1", AddressClass::LinkLocal),
            ("169.254.169.253", AddressClass::LinkLocal),
            ("169.254.169.255", AddressClass::LinkLocal),
            ("169.254.169.254", AddressClass::Metadata),
            ("100.100.100.200", AddressClass::Metadata),
            ("::ffff:100.100.100.200", AddressClass::Metadata),
            ("100.100.100.199", AddressClass::Shared),
            ("100.100.100.201", AddressClass::Shared),
            ("100.100.101.200", AddressClass::Shared),
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
            ("::ffff:169.254.169.254", AddressClass::Metadata),
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
            // The metadata address is exact: its neighbours are plain unique-local.
            ("fd00:ec2::255", "unique-local"),
            ("fd00:ec3::254", "unique-local"),
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
        ] {
            assert_eq!(class_name(public), None, "{public}");
        }
    }

    /// Catches a private entry that exempts more than the private classes: loopback,
    /// link-local, the metadata addresses, unspecified and multicast stay refused, in
    /// IPv6 and IPv4-mapped forms too, while the private classes pass for the listed host.
    #[test]
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
            "100.100.100.200",
            "::ffff:100.100.100.200",
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
            AddressClass::Metadata,
            AddressClass::Private,
            AddressClass::Shared,
            AddressClass::UniqueLocal,
            AddressClass::SiteLocal,
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
                "cloud-metadata",
                "private",
                "shared-range",
                "unique-local",
                "site-local",
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

    /// Catches a loopback exemption that spreads or is missing: a host listed as loopback
    /// reaches 127.0.0.0/8, `::1` and their mapped forms and nothing else that is
    /// refused; the private ranges stay refused for it, and loopback stays refused for
    /// every other host and for a host listed only as private.
    #[test]
    fn a_loopback_entry_exempts_loopback_for_its_host_only() {
        let policy = Policy::new(vec![
            Allow::parse("local.example:5000/team")
                .expect("entry")
                .loopback(),
            Allow::parse("farm.example/team").expect("entry").private(),
        ]);
        let host = |text: &str| Authority::parse(text).expect("authority");
        let ip = |text: &str| -> IpAddr { text.parse().expect("ip") };
        for ok in ["127.0.0.1", "127.9.9.9", "::1", "::ffff:127.0.0.1"] {
            policy
                .address_ok(&host("local.example:5000"), ip(ok))
                .expect(ok);
        }
        for refused in [
            "10.1.2.3",
            "169.254.169.254",
            "169.254.1.1",
            "168.63.129.16",
            "fd00:ec2::254",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            assert!(
                matches!(
                    policy.address_ok(&host("local.example:5000"), ip(refused)),
                    Err(Error::Address { .. })
                ),
                "{refused}"
            );
        }
        for (name, refused) in [
            ("local.example:5001", "127.0.0.1"),
            ("other.example", "127.0.0.1"),
            ("farm.example", "127.0.0.1"),
            ("farm.example", "::1"),
        ] {
            assert!(
                matches!(
                    policy.address_ok(&host(name), ip(refused)),
                    Err(Error::Address { .. })
                ),
                "{name} {refused}"
            );
        }
    }

    /// Catches a realm list that admits a registry or the wrong host: a host added with
    /// `allow_realm` is a token endpoint and not a pin, a port must match, and a bad
    /// host is refused.
    #[test]
    fn a_realm_host_is_a_token_endpoint_and_not_a_registry() {
        let policy = Policy::new(vec![Allow::parse("docker.io/library").expect("entry")])
            .allow_realm("auth.docker.io")
            .expect("realm");
        let host = |text: &str| Authority::parse(text).expect("authority");
        let endpoint = host("registry-1.docker.io");
        assert!(policy.realm_ok(&host("auth.docker.io"), &endpoint));
        assert!(!policy.realm_ok(&host("auth.docker.io:8443"), &endpoint));
        assert!(!policy.realm_ok(&host("evil.example"), &endpoint));
        assert!(policy.admit(&location("auth.docker.io/library/x")).is_err());
        assert!(matches!(
            Policy::default().allow_realm("bad host"),
            Err(Error::Reference(_))
        ));
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
