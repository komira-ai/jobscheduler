//! An endpoint an `allow-list` job may reach (`--egress-allow`): an IPv4 address or a CIDR of
//! `/24` or narrower, and a TCP port.

use std::fmt;
use std::net::Ipv4Addr;

/// The most `--egress-allow` entries accepted.
pub const ALLOW_MAX: usize = 16;

/// The link-local range no endpoint may touch: cloud metadata.
const LINK_LOCAL: (u32, u32) = (0xA9FE_0000, 0xA9FE_FFFF);

/// A TCP endpoint a job may open: an address or a CIDR of `/24` or narrower, and a port.
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

    /// The address, or the network address of the CIDR.
    #[must_use]
    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }

    /// The prefix length: 32 for a single address.
    #[must_use]
    pub fn prefix_len(&self) -> u8 {
        self.len
    }

    /// The TCP port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the endpoint lies in (or holds) `addr/len`.
    #[must_use]
    pub fn overlaps(&self, (addr, len): (Ipv4Addr, u8)) -> bool {
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
        match self.len {
            32 => write!(f, "{}:{}", self.addr, self.port),
            len => write!(f, "{}/{len}:{}", self.addr, self.port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an endpoint wider than `/24`, one with host bits, a port of 0 or above
    /// 65535, a port that is not plain digits, anything in or holding the metadata range,
    /// and an address that is not IPv4: each is refused; a sound one is taken with its
    /// address, prefix and port (a leading zero of the port is read as a number); and the
    /// addresses and `/24`s just outside the metadata range are taken.
    #[test]
    fn an_endpoint_is_ipv4_a_narrow_prefix_and_a_port() {
        for bad in [
            "198.51.100.10",
            "198.51.100.10:",
            "198.51.100.10:0",
            "198.51.100.10:65536",
            "198.51.100.10:x",
            "198.51.100.10:+443",
            "198.51.100.0/23:443",
            "198.51.100.1/24:443",
            "198.51.100.10/33:443",
            "198.51.100.0/x:443",
            "169.254.169.254:80",
            "169.254.0.0/24:80",
            "169.254.255.255:80",
            "0.0.0.0/0:80",
            "[2001:db8::1]:443",
            "2001:db8::1:443",
            "host.example:443",
        ] {
            assert_eq!(Allow::parse(bad), None, "{bad}");
        }
        let one = Allow::parse("198.51.100.10:0443").expect("an address");
        assert_eq!(
            (one.addr(), one.prefix_len(), one.port()),
            (Ipv4Addr::new(198, 51, 100, 10), 32, 443)
        );
        assert_eq!(one.to_string(), "198.51.100.10:443");
        let net = Allow::parse("198.51.100.0/24:8443").expect("a network");
        assert_eq!(
            (net.addr(), net.prefix_len(), net.port()),
            (Ipv4Addr::new(198, 51, 100, 0), 24, 8443)
        );
        assert_eq!(net.to_string(), "198.51.100.0/24:8443");
        let edge = [
            "169.253.255.255:80",
            "169.255.0.0:80",
            "169.253.255.0/24:80",
            "169.255.0.0/24:80",
        ];
        for good in edge {
            assert!(Allow::parse(good).is_some(), "{good}");
        }
    }

    /// Catches an overlap test that looks one way only: a range below, above, inside and
    /// around the endpoint, and one that only touches its edge.
    #[test]
    fn an_endpoint_overlaps_a_range_it_shares_an_address_with() {
        let net = Allow::parse("198.51.100.0/24:443").expect("a network");
        let at = |a: u8, b: u8, c: u8, d: u8, len: u8| (Ipv4Addr::new(a, b, c, d), len);
        assert!(!net.overlaps(at(198, 51, 99, 0, 24)), "below");
        assert!(!net.overlaps(at(198, 51, 101, 0, 24)), "above");
        assert!(net.overlaps(at(198, 51, 100, 7, 32)), "inside");
        assert!(net.overlaps(at(198, 51, 0, 0, 16)), "around");
        assert!(net.overlaps(at(198, 51, 100, 255, 32)), "the last address");
        assert!(net.overlaps(at(0, 0, 0, 0, 0)), "everything");
        let one = Allow::parse("198.51.100.10:443").expect("an address");
        assert!(one.overlaps(at(198, 51, 100, 10, 32)));
        assert!(!one.overlaps(at(198, 51, 100, 11, 32)));
    }
}
