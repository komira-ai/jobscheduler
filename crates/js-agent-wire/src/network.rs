//! The network of a container job on the wire (design 5.8): the ports a start declares,
//! checked by [`JobSpec::validate`](crate::JobSpec::validate), and the ceiling an agent
//! reports in its hello ([`NetworkCeiling`], under the capabilities [`EGRESS_CAPABILITY`] and
//! [`EGRESS_PORTS_CAPABILITY`]). The agent writes the ceiling with
//! [`NetworkCeiling::insert_into`] and a server reads it with
//! [`NetworkCeiling::from_capabilities`], so both sides read the one text.
//!
//! The level itself is [`js_types::JobNetwork`] and a declared port [`js_types::PortMap`].

use std::collections::BTreeMap;
use std::fmt;

use js_types::{JobNetwork, PortMap};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// The capability an agent reports its network ceiling under: `none`, `allow-list` or
/// `open`, the most a job it runs may ask for (`--egress`). Absent reads as `none`.
pub const EGRESS_CAPABILITY: &str = "egress";

/// The capability an agent reports the host ports a job may declare under: a comma-joined
/// list of `N` and `LO-HI` items, at most [`limits::MAX_PORTS`] of them
/// (`--egress-forward-port`). Absent reads as no port.
pub const EGRESS_PORTS_CAPABILITY: &str = "egress-ports";

/// The field each declared host port is refused under, by its place in the list.
const HOST_FIELDS: [&str; limits::MAX_PORTS] = [
    "ports[0].host",
    "ports[1].host",
    "ports[2].host",
    "ports[3].host",
    "ports[4].host",
    "ports[5].host",
    "ports[6].host",
    "ports[7].host",
    "ports[8].host",
    "ports[9].host",
    "ports[10].host",
    "ports[11].host",
    "ports[12].host",
    "ports[13].host",
    "ports[14].host",
    "ports[15].host",
];

/// The field each declared container port is refused under, by its place in the list.
const CONTAINER_FIELDS: [&str; limits::MAX_PORTS] = [
    "ports[0].container",
    "ports[1].container",
    "ports[2].container",
    "ports[3].container",
    "ports[4].container",
    "ports[5].container",
    "ports[6].container",
    "ports[7].container",
    "ports[8].container",
    "ports[9].container",
    "ports[10].container",
    "ports[11].container",
    "ports[12].container",
    "ports[13].container",
    "ports[14].container",
    "ports[15].container",
];

/// The declared ports of a start: at most [`limits::MAX_PORTS`] (`ports`), each host and
/// container port above zero (`ports[n].host`, `ports[n].container`), and no host port twice
/// (`ports[n].host`, the later one), so no container port is named twice for one host port
/// either. A host port below 1024 is allowed here; the agent's ceiling decides. Refused
/// under `invalid-job`.
pub(crate) fn check_ports(ports: &[PortMap]) -> Result<(), ValidationError> {
    let at = At::new("ports", refusal::INVALID_JOB);
    check::count(at, ports.len(), limits::MAX_PORTS)?;
    for (n, port) in ports.iter().enumerate() {
        let host = at.field(HOST_FIELDS[n]);
        if port.host == 0 {
            return Err(host.fail(Rule::OutOfRange));
        }
        if port.container == 0 {
            return Err(at.field(CONTAINER_FIELDS[n]).fail(Rule::OutOfRange));
        }
        if ports[..n].iter().any(|earlier| earlier.host == port.host) {
            return Err(host.fail(Rule::Duplicate));
        }
    }
    Ok(())
}

/// A range of host ports, `lo` to `hi`, both included, from 1 to 65535: one item of
/// `--egress-forward-port` and of the capability [`EGRESS_PORTS_CAPABILITY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortRange {
    lo: u16,
    hi: u16,
}

impl PortRange {
    /// The ports `lo` to `hi`; `None` unless `1 <= lo <= hi`.
    #[must_use]
    pub fn new(lo: u16, hi: u16) -> Option<Self> {
        (1..=hi).contains(&lo).then_some(Self { lo, hi })
    }

    /// `N` (one port) or `LO-HI`, each a decimal number from 1 to 65535 with no sign, space
    /// or other character, and `LO` not above `HI`; `None` for anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (lo, hi) = text.split_once('-').unwrap_or((text, text));
        Self::new(port_number(lo)?, port_number(hi)?)
    }

    /// The first port.
    #[must_use]
    pub fn lo(self) -> u16 {
        self.lo
    }

    /// The last port.
    #[must_use]
    pub fn hi(self) -> u16 {
        self.hi
    }

    /// Whether `port` is in the range.
    #[must_use]
    pub fn contains(self, port: u16) -> bool {
        (self.lo..=self.hi).contains(&port)
    }
}

impl fmt::Display for PortRange {
    /// `N` for one port, `LO-HI` for more: what [`PortRange::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.lo == self.hi {
            true => write!(f, "{}", self.lo),
            false => write!(f, "{}-{}", self.lo, self.hi),
        }
    }
}

/// A decimal port number of one to five digits that fits a `u16`.
fn port_number(text: &str) -> Option<u16> {
    if !(1..=5).contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The most network the jobs of a host may ask for: the agent's level and the host ports a
/// job may declare (design 5.8). A job above it is refused at start
/// ([`refusal::NETWORK_ABOVE_CEILING`], [`refusal::PORT_ABOVE_CEILING`]); it is never run
/// with less than it asked for.
///
/// A hello carries it in its capabilities ([`EGRESS_CAPABILITY`], [`EGRESS_PORTS_CAPABILITY`]).
/// An agent that predates the keys reports neither, and that reads as level `none` with no
/// port. Such an agent also ignores a start's `network` and `ports` and would run the job
/// with less than it asked for, so a server must place a job whose `network` is not `none`,
/// or that declares a port, only on a host whose ceiling covers it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkCeiling {
    /// The most network a job may ask for.
    pub level: JobNetwork,
    /// The host ports a job may declare; none by default.
    pub ports: Vec<PortRange>,
}

impl NetworkCeiling {
    /// Writes the ceiling into a hello's `capabilities`: [`EGRESS_CAPABILITY`] always, and
    /// [`EGRESS_PORTS_CAPABILITY`] when it names a port.
    pub fn insert_into(&self, capabilities: &mut BTreeMap<String, String>) {
        capabilities.insert(EGRESS_CAPABILITY.to_owned(), self.level.name().to_owned());
        if !self.ports.is_empty() {
            capabilities.insert(EGRESS_PORTS_CAPABILITY.to_owned(), self.ports_text());
        }
    }

    /// The ceiling a hello's `capabilities` report; an absent key reads as level `none` and
    /// no port, as an agent from before the keys has it.
    ///
    /// # Errors
    ///
    /// `invalid-hello` naming the key: `capabilities.egress` is not a level;
    /// `capabilities.egress-ports` is not a list of `N` and `LO-HI` items, holds more than
    /// [`limits::MAX_PORTS`] of them, or is given with the level `none` (no job there has an
    /// interface a port could reach).
    pub fn from_capabilities(
        capabilities: &BTreeMap<String, String>,
    ) -> Result<Self, ValidationError> {
        let named = At::new("capabilities.egress", refusal::INVALID_HELLO);
        let level = match capabilities.get(EGRESS_CAPABILITY) {
            None => JobNetwork::None,
            Some(text) => text.parse().map_err(|_| named.fail(Rule::BadChars))?,
        };
        let at = At::new("capabilities.egress-ports", refusal::INVALID_HELLO);
        let Some(text) = capabilities.get(EGRESS_PORTS_CAPABILITY) else {
            return Ok(Self {
                level,
                ports: Vec::new(),
            });
        };
        let ports = text
            .split(',')
            .map(PortRange::parse)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| at.fail(Rule::BadChars))?;
        check::count(at, ports.len(), limits::MAX_PORTS)?;
        if level.is_none() {
            return Err(at.fail(Rule::Inconsistent));
        }
        Ok(Self { level, ports })
    }

    /// Whether a job may ask for `level`: it is not above the ceiling's.
    #[must_use]
    pub fn allows(&self, level: JobNetwork) -> bool {
        level <= self.level
    }

    /// Whether a job may declare the host port `port`.
    #[must_use]
    pub fn forwards(&self, port: u16) -> bool {
        self.ports.iter().any(|range| range.contains(port))
    }

    /// The ports as the capability [`EGRESS_PORTS_CAPABILITY`] says them: the items joined
    /// with `,`; empty for none.
    #[must_use]
    pub fn ports_text(&self) -> String {
        let items: Vec<String> = self.ports.iter().map(ToString::to_string).collect();
        items.join(",")
    }
}
