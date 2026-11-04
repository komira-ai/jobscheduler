//! What network a job asks for (design 5.8): a level, [`JobNetwork`], and the ports it
//! declares, [`PortMap`]. The agent's flags are the ceiling a job cannot exceed; a job that
//! asks for more is refused, never run with less.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much network a job's container has. The levels are ordered, `None < AllowList <
/// Open`, so a level is within a ceiling when it is not above it. Text and the wire spell
/// them `none`, `allow-list` and `open`; a text this version does not know is refused, so
/// a job is never run at a level it did not name.
///
/// It is not the `NetworkLevel` of a native job on the agent wire (`off`, `loopback`, `on`),
/// which is a macOS sandbox setting.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum JobNetwork {
    /// No route out: loopback only, in the job's own network namespace. The default.
    #[default]
    None,
    /// Only the endpoints and resolvers the agent is configured with; the job picks the level
    /// and cannot add endpoints.
    AllowList,
    /// Any address except what the agent always denies: the cloud metadata range
    /// `169.254.0.0/16`, IPv6, and spoofed source addresses.
    Open,
}

impl JobNetwork {
    /// The text that names the level: `none`, `allow-list` or `open`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AllowList => "allow-list",
            Self::Open => "open",
        }
    }

    /// Whether this is [`JobNetwork::None`], the level a field that is left out reads as.
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

impl fmt::Display for JobNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A text that names no network level.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown network level {0:?}; known: none, allow-list, open")]
pub struct UnknownJobNetwork(pub String);

impl FromStr for JobNetwork {
    type Err = UnknownJobNetwork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "allow-list" => Ok(Self::AllowList),
            "open" => Ok(Self::Open),
            _ => Err(UnknownJobNetwork(s.to_owned())),
        }
    }
}

/// A port a job declares: traffic to the host's port `host` is to reach the job's port
/// `container`. Both are above zero; the agent wire refuses a start that breaks that
/// (`JobSpec::validate`). A host port is the agent's to allow: its flags list the host ports
/// a job may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PortMap {
    /// The host's port, from 1 to 65535.
    pub host: u16,
    /// The job's port inside its container, from 1 to 65535.
    pub container: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every level prints as the text the wire and the flags spell it, parses
    /// back from it, and that any other text (another case, the native levels, empty) is
    /// refused naming what is known. Catches a level that is spelled one way and read
    /// another, and a parser that lowers an unknown text to a known level.
    #[test]
    fn levels_round_trip_and_an_unknown_text_is_refused() {
        for level in [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open] {
            assert_eq!(level.name().parse::<JobNetwork>(), Ok(level));
            assert_eq!(level.to_string(), level.name());
        }
        assert_eq!(JobNetwork::AllowList.name(), "allow-list");
        for text in ["", "Open", "allow_list", "on", "off", "loopback", "wide"] {
            assert_eq!(
                text.parse::<JobNetwork>(),
                Err(UnknownJobNetwork(text.to_owned()))
            );
        }
        let said = UnknownJobNetwork("x".into()).to_string();
        assert!(said.contains("none, allow-list, open"), "{said}");
    }

    /// Asserts the order the ceiling comparison relies on, `none < allow-list < open`, and
    /// that only `none` reads as none. Catches a reordered enum, which would let a job above
    /// the ceiling through.
    #[test]
    fn the_levels_are_ordered_none_allow_list_open() {
        assert!(JobNetwork::None < JobNetwork::AllowList);
        assert!(JobNetwork::AllowList < JobNetwork::Open);
        assert!(JobNetwork::None.is_none());
        assert!(!JobNetwork::AllowList.is_none());
        assert!(!JobNetwork::Open.is_none());
    }

    /// Asserts the wire spelling of each level and of a port, and that an unknown level is a
    /// decode error, never a level. Catches a lost `kebab-case` (`allow_list` or `AllowList`
    /// on the wire) and a field renamed in a port.
    #[test]
    fn the_wire_spells_the_levels_and_ports() {
        let text =
            serde_json::to_string(&[JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open])
                .unwrap();
        assert_eq!(text, r#"["none","allow-list","open"]"#);
        assert!(serde_json::from_str::<JobNetwork>(r#""wide""#).is_err());
        let port = PortMap {
            host: 8080,
            container: 80,
        };
        let text = serde_json::to_string(&port).unwrap();
        assert_eq!(text, r#"{"host":8080,"container":80}"#);
        assert_eq!(serde_json::from_str::<PortMap>(&text).unwrap(), port);
    }
}
