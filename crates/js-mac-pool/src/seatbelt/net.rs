//! The network levels of a run.

use std::collections::BTreeSet;

/// The most loopback ports one policy lists; each is a rule of its own in the profile.
pub const MAX_PORTS: usize = 64;

/// Why a port list is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PortsError {
    /// The list is empty; "no port" is [`Net::Off`].
    #[error("no ports listed")]
    Empty,
    /// Port 0 is not a port a peer listens on.
    #[error("port 0 listed")]
    Zero,
    /// More than [`MAX_PORTS`] ports.
    #[error("too many ports listed")]
    TooMany,
}

/// A non-empty set of TCP ports: loopback ports under [`Net::Ports`], ports on any address
/// under [`Net::Remote`]. Single ports only: the profile compiler rejects
/// `localhost:LOW-HIGH`, so a range is spelled as its ports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ports(BTreeSet<u16>);

impl Ports {
    /// The set of `ports`.
    ///
    /// # Errors
    /// [`PortsError`] for an empty list, a zero port or more than [`MAX_PORTS`] distinct
    /// ports.
    pub fn new(ports: &[u16]) -> Result<Self, PortsError> {
        let set: BTreeSet<u16> = ports.iter().copied().collect();
        if set.is_empty() {
            return Err(PortsError::Empty);
        }
        if set.contains(&0) {
            return Err(PortsError::Zero);
        }
        if set.len() > MAX_PORTS {
            return Err(PortsError::TooMany);
        }
        Ok(Self(set))
    }

    /// The ports, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().copied()
    }

    fn shared(&self, other: &Self) -> Vec<u16> {
        self.0.intersection(&other.0).copied().collect()
    }
}

/// What a run may reach on the network. The levels are ordered
/// `Off < Ports < Loopback < Remote < On`; a request and the agent's cap both apply and the
/// lower wins ([`Net::narrow`]).
///
/// Measured on a hosted runner: `Off` cuts loopback, outbound connects and name
/// resolution, and `Loopback` cuts name resolution too (only `On` resolves names).
/// Loopback is shared by every user of the Mac, so concurrent runs can reach each
/// other's listeners at `Ports` and `Loopback`; that is a channel among concurrent runs
/// of one consumer, not a boundary between tenants. The host's own services are denied by
/// port at every level but `Off` ([`Narrowing::deny_ports`](super::Narrowing::deny_ports)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Net {
    /// Nothing: no loopback, no outbound, no DNS. One unix socket by path (the system
    /// log), never the class.
    Off,
    /// `Off` plus connects to these loopback ports.
    Ports(Ports),
    /// `Off` plus any loopback connect and loopback listeners.
    Loopback,
    /// `Loopback` plus TCP connects to these ports on any address, and name lookups (the
    /// `mDNSResponder` socket, by path: name resolution goes through it, and `Off` and
    /// `Loopback` cut it). As under `On`, no address can be denied: the profile compiler
    /// takes only `*` and `localhost` as a host, so the scheduler and the cloud metadata
    /// address are reachable on these ports unless a packet filter stops them.
    Remote(Ports),
    /// The network is open: the profile has no network rule. Seatbelt cannot deny a
    /// literal address (the compiler accepts only `*` and `localhost` as the host of a
    /// `remote ip` filter), so egress control is a packet filter keyed on the pool's user
    /// range (`js-mac-provision`). The agent requires that filter in force for its
    /// `--native-network on` and not for `--native-network open`, which both give a run this
    /// profile: without the filter a run at this level can reach any address the Mac can,
    /// the scheduler and the cloud metadata address included.
    On,
}

impl Net {
    /// The lower of two levels. Two port lists of one level meet in the ports they share;
    /// no shared port is the level below (`Off` below `Ports`, `Loopback` below `Remote`).
    #[must_use]
    pub fn narrow(self, other: Self) -> Self {
        match (self, other) {
            (Self::Ports(a), Self::Ports(b)) => {
                Ports::new(&a.shared(&b)).map_or(Self::Off, Self::Ports)
            }
            (Self::Remote(a), Self::Remote(b)) => {
                Ports::new(&a.shared(&b)).map_or(Self::Loopback, Self::Remote)
            }
            (a, b) if a.rank() <= b.rank() => a,
            (_, b) => b,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Ports(_) => 1,
            Self::Loopback => 2,
            Self::Remote(_) => 3,
            Self::On => 4,
        }
    }
}
