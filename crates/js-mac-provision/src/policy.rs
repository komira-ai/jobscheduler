//! What the anchor is told: the pool's uids, the endpoints they may reach and the
//! anchor's name. Each type is checked when it is made, so the renderer cannot be handed
//! a policy that would write a rule wider than the operator meant.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::str::FromStr;

/// The lowest uid a range may start at: macOS keeps the uids below 500 for system
/// accounts and gives people uids from 501.
pub const MIN_UID: u32 = 500;
/// The highest uid a range may end at: `u32::MAX` is `(uid_t)-1` and the one below it is
/// the account `nobody` has on macOS.
pub const MAX_UID: u32 = u32::MAX - 2;
/// The most uids a range may hold. pf's `user` takes a list and expands every rule over
/// every listed uid, so the anchor grows with the pool; a pool is about ten uids and this
/// leaves room for more without letting a typo write thousands of rules.
pub const MAX_POOL_UIDS: u32 = 128;
/// The longest anchor name, in bytes.
pub const MAX_ANCHOR_LEN: usize = 128;
/// The longest component of an anchor name, in bytes (pf's own limit is 63).
pub const MAX_COMPONENT_LEN: usize = 63;

/// Why a policy was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The range holds more uids than [`MAX_POOL_UIDS`].
    #[error("a uid range of {count} is over the {MAX_POOL_UIDS} a pool may hold")]
    TooManyUids {
        /// How many uids were asked for.
        count: u32,
    },
    /// The range holds no uid.
    #[error("the uid range is empty (a count of 0)")]
    EmptyRange,
    /// The range starts among the system accounts.
    #[error(
        "uid {start} is below {MIN_UID}: those uids are system accounts and a pool never owns them"
    )]
    SystemUids {
        /// The first uid asked for.
        start: u32,
    },
    /// The range runs past the last uid a user can have.
    #[error("uid range of {count} from {start} ends past {MAX_UID}")]
    UidOverflow {
        /// The first uid asked for.
        start: u32,
        /// How many uids were asked for.
        count: u32,
    },
    /// An endpoint is not an IPv4 literal with a port.
    #[error("endpoint {0:?} is not an IPv4 address and a port, as 192.0.2.10:443")]
    NotIpv4Endpoint(String),
    /// An endpoint names port 0.
    #[error("endpoint {0} names port 0")]
    ZeroPort(Ipv4Addr),
    /// An endpoint is an address a pool user must not be allowed to reach by name.
    #[error("endpoint address {addr} is {why}")]
    Unreachable {
        /// The address.
        addr: Ipv4Addr,
        /// What is wrong with it.
        why: &'static str,
    },
    /// An anchor name pf would not take, or one that could name more than one anchor.
    #[error(
        "anchor name {0:?} is not one to {MAX_ANCHOR_LEN} bytes of components [A-Za-z0-9._-] (at most {MAX_COMPONENT_LEN} each) joined by '/'"
    )]
    BadAnchor(String),
}

/// A range of uids, `start` and the `count - 1` after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    start: u32,
    last: u32,
}

impl UidRange {
    /// A range of `count` uids from `start`.
    ///
    /// # Errors
    /// An empty range, one that starts below [`MIN_UID`], or one that ends past
    /// [`MAX_UID`].
    pub fn new(start: u32, count: u32) -> Result<Self, PolicyError> {
        let span = count.checked_sub(1).ok_or(PolicyError::EmptyRange)?;
        if start < MIN_UID {
            return Err(PolicyError::SystemUids { start });
        }
        let last = start
            .checked_add(span)
            .filter(|last| *last <= MAX_UID)
            .ok_or(PolicyError::UidOverflow { start, count })?;
        if count > MAX_POOL_UIDS {
            return Err(PolicyError::TooManyUids { count });
        }
        Ok(Self { start, last })
    }

    /// The first uid.
    #[must_use]
    pub fn first(self) -> u32 {
        self.start
    }

    /// The last uid.
    #[must_use]
    pub fn last(self) -> u32 {
        self.last
    }

    /// Whether `uid` is in the range.
    #[must_use]
    pub fn contains(self, uid: u32) -> bool {
        (self.start..=self.last).contains(&uid)
    }

    /// The range as macOS pf's `user` takes it: `600` for one uid, `{ 600, 601, 602 }`
    /// (every uid, both ends included) for more.
    ///
    /// pf.conf(5) says the `:` range "does not work for user and group match", so macOS
    /// reads `600:602` as a user name and refuses it ("unknown user"). The `uid_item`
    /// rule in pfctl's `parse.y` (OpenBSD, which Apple's pfctl derives from) takes a
    /// uid, a unary operator and a uid, or `uid PORTBINARY uid`, and `uids` takes a
    /// braced list of those. The binary operators are `><` (excludes both ends: `2000 >< 2004`
    /// is 2001 to 2003), `<>` (the "except" range: below 2000 or above 2004) and `:`
    /// (inclusive, unusable here). A list needs no operator whose edges could be
    /// misread, so the list is used; `UidRange` bounds it at [`MAX_POOL_UIDS`].
    pub(crate) fn pf_user(self) -> String {
        if self.start == self.last {
            return self.start.to_string();
        }
        let uids: Vec<String> = (self.start..=self.last).map(|u| u.to_string()).collect();
        format!("{{ {} }}", uids.join(", "))
    }
}

impl fmt::Display for UidRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.last)
    }
}

/// An IPv4 address and a TCP port a pool user may connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    addr: Ipv4Addr,
    port: u16,
}

impl Endpoint {
    /// An endpoint.
    ///
    /// # Errors
    /// Port 0, or an address that is unspecified, loopback, link-local (the whole of
    /// 169.254.0.0/16, which holds the cloud metadata address), multicast or broadcast.
    pub fn new(addr: Ipv4Addr, port: u16) -> Result<Self, PolicyError> {
        let why = if addr.is_unspecified() {
            Some("the unspecified address")
        } else if addr.is_loopback() {
            Some("loopback, which the anchor handles on its own")
        } else if addr.is_link_local() {
            Some("link-local (169.254.0.0/16), where the cloud metadata address is")
        } else if addr.is_multicast() {
            Some("multicast")
        } else if addr.is_broadcast() {
            Some("the broadcast address")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(PolicyError::Unreachable { addr, why });
        }
        if port == 0 {
            return Err(PolicyError::ZeroPort(addr));
        }
        Ok(Self { addr, port })
    }

    /// The address.
    #[must_use]
    pub fn addr(self) -> Ipv4Addr {
        self.addr
    }

    /// The port.
    #[must_use]
    pub fn port(self) -> u16 {
        self.port
    }
}

impl FromStr for Endpoint {
    type Err = PolicyError;

    /// `192.0.2.10:443`: an IPv4 literal. A host name or an IPv6 address is refused:
    /// a name would need DNS, which the anchor does not let a pool user do.
    fn from_str(text: &str) -> Result<Self, PolicyError> {
        let socket: SocketAddrV4 = text
            .parse()
            .map_err(|_| PolicyError::NotIpv4Endpoint(text.to_owned()))?;
        Self::new(*socket.ip(), socket.port())
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.addr, self.port)
    }
}

/// A pf anchor path: components of `[A-Za-z0-9._-]` joined by `/`, as
/// `com.apple/com.komira.js`. A wildcard, a quote or a space would let the name stand
/// for more than one anchor or end the string the renderer and the plist put it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorName(String);

impl AnchorName {
    /// An anchor name.
    ///
    /// # Errors
    /// [`PolicyError::BadAnchor`].
    pub fn new(name: &str) -> Result<Self, PolicyError> {
        let component = |c: &str| {
            !c.is_empty()
                && c.len() <= MAX_COMPONENT_LEN
                && c != "."
                && c != ".."
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        if name.len() > MAX_ANCHOR_LEN || !name.split('/').all(component) {
            return Err(PolicyError::BadAnchor(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AnchorName {
    type Err = PolicyError;

    fn from_str(name: &str) -> Result<Self, PolicyError> {
        Self::new(name)
    }
}

impl fmt::Display for AnchorName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_of_one_is_a_single_uid() {
        let r = UidRange::new(600, 1).unwrap();
        assert_eq!((r.first(), r.last()), (600, 600));
        assert_eq!(r.pf_user(), "600");
        assert_eq!(r.to_string(), "600-600");
    }

    #[test]
    fn a_range_runs_count_uids_from_start() {
        let r = UidRange::new(600, 100).unwrap();
        assert_eq!((r.first(), r.last()), (600, 699));
        assert!(r.pf_user().starts_with("{ 600, 601, 602, "));
        assert!(r.pf_user().ends_with(", 698, 699 }"));
        assert!(r.contains(600));
        assert!(r.contains(699));
        assert!(!r.contains(599));
        assert!(!r.contains(700));
    }

    #[test]
    fn pf_user_lists_each_uid_of_a_range_and_never_uses_a_colon() {
        assert_eq!(UidRange::new(4100, 1).unwrap().pf_user(), "4100");
        assert_eq!(UidRange::new(4100, 2).unwrap().pf_user(), "{ 4100, 4101 }");
        assert_eq!(
            UidRange::new(4100, 10).unwrap().pf_user(),
            "{ 4100, 4101, 4102, 4103, 4104, 4105, 4106, 4107, 4108, 4109 }"
        );
        for count in [1, 2, 10, 100] {
            let user = UidRange::new(4100, count).unwrap().pf_user();
            assert!(!user.contains(':'));
            let listed = user.trim_start_matches("{ ").trim_end_matches(" }");
            assert_eq!(listed.split(", ").count(), count as usize, "{user}");
        }
    }

    #[test]
    fn a_pool_is_bounded_so_the_rule_list_stays_small() {
        assert!(UidRange::new(600, 128).is_ok());
        assert!(UidRange::new(600, 129).is_err());
    }

    #[test]
    fn a_range_refuses_nothing_and_the_system_uids_and_the_end_of_the_space() {
        assert_eq!(UidRange::new(600, 0), Err(PolicyError::EmptyRange));
        assert_eq!(
            UidRange::new(499, 10),
            Err(PolicyError::SystemUids { start: 499 })
        );
        assert_eq!(
            UidRange::new(0, 1000),
            Err(PolicyError::SystemUids { start: 0 })
        );
        assert!(UidRange::new(500, 1).is_ok());
        assert_eq!(
            UidRange::new(MAX_UID, 2),
            Err(PolicyError::UidOverflow {
                start: MAX_UID,
                count: 2
            })
        );
        assert_eq!(
            UidRange::new(600, u32::MAX),
            Err(PolicyError::UidOverflow {
                start: 600,
                count: u32::MAX
            })
        );
        assert!(UidRange::new(MAX_UID, 1).is_ok());
    }

    #[test]
    fn an_endpoint_is_an_ipv4_literal_and_a_port() {
        let e: Endpoint = "192.0.2.10:443".parse().unwrap();
        assert_eq!((e.addr(), e.port()), (Ipv4Addr::new(192, 0, 2, 10), 443));
        assert_eq!(e.to_string(), "192.0.2.10:443");
    }

    #[test]
    fn an_endpoint_that_is_not_an_ipv4_literal_is_refused() {
        for text in [
            "",
            "example.com:443",
            "192.0.2.10",
            "[2001:db8::1]:443",
            "2001:db8::1",
            "192.0.2.10:99999",
            " 192.0.2.10:443",
        ] {
            assert_eq!(
                text.parse::<Endpoint>(),
                Err(PolicyError::NotIpv4Endpoint(text.to_owned())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn an_endpoint_on_an_address_the_anchor_must_not_open_is_refused() {
        for (text, why) in [
            ("0.0.0.0:443", "unspecified"),
            ("127.0.0.1:443", "loopback"),
            ("169.254.169.254:80", "link-local"),
            ("169.254.0.1:80", "link-local"),
            ("169.254.255.255:80", "link-local"),
            ("224.0.0.251:5353", "multicast"),
            ("255.255.255.255:443", "broadcast"),
        ] {
            let err = text.parse::<Endpoint>().unwrap_err();
            assert!(matches!(err, PolicyError::Unreachable { .. }), "{text}");
            assert!(err.to_string().contains(why), "{text}: {err}");
        }
        // The neighbours of 169.254.0.0/16 are ordinary addresses.
        assert!("169.253.255.255:80".parse::<Endpoint>().is_ok());
        assert!("169.255.0.0:80".parse::<Endpoint>().is_ok());
    }

    #[test]
    fn an_endpoint_with_port_zero_is_refused() {
        assert_eq!(
            "192.0.2.10:0".parse::<Endpoint>(),
            Err(PolicyError::ZeroPort(Ipv4Addr::new(192, 0, 2, 10)))
        );
    }

    #[test]
    fn endpoints_order_by_address_then_port() {
        let mut v: Vec<Endpoint> = ["198.51.100.1:443", "192.0.2.10:8443", "192.0.2.10:443"]
            .iter()
            .map(|t| t.parse().unwrap())
            .collect();
        v.sort();
        let shown: Vec<String> = v.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            ["192.0.2.10:443", "192.0.2.10:8443", "198.51.100.1:443"]
        );
    }

    #[test]
    fn an_anchor_name_is_components_joined_by_slashes() {
        for name in [
            "com.komira.js",
            "com.apple/com.komira.js.pool",
            "a/b_c-d/e.f",
        ] {
            let a: AnchorName = name.parse().unwrap();
            assert_eq!(a.as_str(), name);
            assert_eq!(a.to_string(), name);
        }
    }

    #[test]
    fn an_anchor_name_that_could_mean_more_than_one_anchor_is_refused() {
        let long_component = "a".repeat(MAX_COMPONENT_LEN + 1);
        let long_name = vec!["a"; MAX_ANCHOR_LEN].join("/");
        for name in [
            "",
            "/",
            "/a",
            "a/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "com.apple/*",
            "a b",
            "a\"b",
            "a\nb",
            "caf\u{e9}",
            &long_component,
            &long_name,
        ] {
            assert_eq!(
                AnchorName::new(name),
                Err(PolicyError::BadAnchor(name.to_owned())),
                "{name:?}"
            );
        }
        assert!(AnchorName::new(&"a".repeat(MAX_COMPONENT_LEN)).is_ok());
    }

    #[test]
    fn every_error_says_what_to_change() {
        let e = PolicyError::SystemUids { start: 10 };
        assert!(e.to_string().contains("below 500"));
        assert!(PolicyError::EmptyRange.to_string().contains("empty"));
    }
}
