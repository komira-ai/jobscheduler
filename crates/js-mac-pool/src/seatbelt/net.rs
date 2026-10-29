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

/// A non-empty set of loopback TCP ports. Single ports only: the profile compiler
/// rejects `localhost:LOW-HIGH`, so a range is spelled as its ports.
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
}

/// What a run may reach on the network. The levels are ordered
/// `Off < Ports < Loopback < On`; a request and the agent's cap both apply and the lower
/// wins ([`Net::narrow`]).
///
/// Measured on a hosted runner: `Off` cuts loopback, outbound connects and name
/// resolution, and `Loopback` cuts name resolution too (only `On` resolves names).
/// Loopback is shared by every user of the Mac, so concurrent runs can reach each
/// other's listeners at `Ports` and `Loopback`; that is a channel among concurrent runs
/// of one consumer, not a boundary between tenants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Net {
    /// Nothing: no loopback, no outbound, no DNS. One unix socket by path (the system
    /// log), never the class.
    Off,
    /// `Off` plus connects to these loopback ports.
    Ports(Ports),
    /// `Off` plus any loopback connect and loopback listeners.
    Loopback,
    /// The network is open, minus the denied egress addresses of the policy.
    On,
}

impl Net {
    /// The lower of two levels. Two port lists meet in the ports they share; no shared
    /// port is `Off`.
    #[must_use]
    pub fn narrow(self, other: Self) -> Self {
        match (self, other) {
            (Self::Ports(a), Self::Ports(b)) => {
                let shared: Vec<u16> = a.0.intersection(&b.0).copied().collect();
                Ports::new(&shared).map_or(Self::Off, Self::Ports)
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
            Self::On => 3,
        }
    }
}
