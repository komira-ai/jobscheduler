//! The network a host's agent advertises, and whether a job's network and host ports are
//! within it (design 5.8: the agent advertises its ceiling, and a job that needs more than
//! a host's ceiling finds that host not capable; design 5.6, step 2).
//!
//! A host's ceiling is read from the capabilities its hello reported
//! ([`js_agent_wire::NetworkCeiling`]). An agent that predates the keys reports neither, and
//! reads as level `none` with no port, so a job that asks for a network or declares a port is
//! never placed on it. This module reads no clock and keeps no state.

use js_agent_wire::NetworkCeiling;
use js_types::{Capabilities, JobNetwork};

/// The ceiling `capabilities` advertise. Absent keys read as level `none` and no port, and so
/// do capabilities that cannot be read as a ceiling (the heartbeat path reads them the same
/// way).
#[must_use]
pub fn ceiling_of(capabilities: &Capabilities) -> NetworkCeiling {
    NetworkCeiling::from_capabilities(capabilities).unwrap_or_default()
}

/// Why a job is above a host's ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BeyondCeiling {
    /// The job asks for more network than the host allows.
    #[error("network {asked} is above the host's ceiling {ceiling}")]
    Network {
        /// The level the job asks for.
        asked: JobNetwork,
        /// The level the host allows.
        ceiling: JobNetwork,
    },
    /// The job declares a host port the host does not forward.
    #[error("host port {0} is not one the host forwards")]
    Port(u16),
}

/// Whether a job asking for `network` and declaring `host_ports` is within `ceiling`: its
/// level is not above the ceiling's and the host forwards every port.
///
/// # Errors
///
/// [`BeyondCeiling::Network`] when the level is above the ceiling's, checked first;
/// otherwise [`BeyondCeiling::Port`] for the first of `host_ports`, in the order given, that
/// the host does not forward.
pub fn within_ceiling(
    ceiling: &NetworkCeiling,
    network: JobNetwork,
    host_ports: &[u16],
) -> Result<(), BeyondCeiling> {
    if !ceiling.allows(network) {
        return Err(BeyondCeiling::Network {
            asked: network,
            ceiling: ceiling.level,
        });
    }
    match host_ports.iter().find(|port| !ceiling.forwards(**port)) {
        Some(port) => Err(BeyondCeiling::Port(*port)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use js_agent_wire::PortRange;

    use super::*;

    fn caps(pairs: &[(&str, &str)]) -> Capabilities {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// Asserts that capabilities without the egress keys, or whose egress keys cannot be
    /// read, read as level `none` with no port, and that a readable pair is parsed. Catches a
    /// host from before the keys, or one with a broken value, read as having a network.
    #[test]
    fn capabilities_without_the_keys_or_unreadable_read_as_ceiling_none() {
        let none = NetworkCeiling::default();
        assert_eq!(ceiling_of(&Capabilities::new()), none);
        assert_eq!(ceiling_of(&caps(&[("os", "linux")])), none);
        let read = ceiling_of(&caps(&[
            ("egress", "allow-list"),
            ("egress-ports", "80,8000-8010"),
        ]));
        assert_eq!(read.level, JobNetwork::AllowList);
        let eighty = PortRange::new(80, 80).unwrap();
        let eight_thousand = PortRange::new(8000, 8010).unwrap();
        assert_eq!(read.ports, vec![eighty, eight_thousand]);
        for unreadable in [
            caps(&[("egress-ports", "80")]),
            caps(&[("egress", "none"), ("egress-ports", "80")]),
            caps(&[("egress", "wide")]),
            caps(&[("egress", "open"), ("egress-ports", "eighty")]),
        ] {
            assert_eq!(ceiling_of(&unreadable), none, "{unreadable:?}");
        }
    }

    /// Asserts that a job above the level names the level and the ceiling, that a port the
    /// host does not forward is named (the first such, in the order given), that the level is
    /// checked before the ports, that the ends of a range are inside and the ports just past
    /// them outside, and that a job with no port within the level is within. Catches a check
    /// that reads a range as open at an end, or reports the wrong refusal first.
    #[test]
    fn within_ceiling_names_the_level_or_the_first_port_outside() {
        let none = NetworkCeiling::default();
        let above = within_ceiling(&none, JobNetwork::Open, &[8080]).unwrap_err();
        assert_eq!(
            above,
            BeyondCeiling::Network {
                asked: JobNetwork::Open,
                ceiling: JobNetwork::None,
            }
        );
        assert_eq!(
            above.to_string(),
            "network open is above the host's ceiling none"
        );
        let pairs = [("egress", "allow-list"), ("egress-ports", "8000-8010")];
        let ranged = ceiling_of(&caps(&pairs));
        let listed = [8000, 8080, 9000];
        let outside = within_ceiling(&ranged, JobNetwork::AllowList, &listed).unwrap_err();
        assert_eq!(outside, BeyondCeiling::Port(8080));
        assert_eq!(
            outside.to_string(),
            "host port 8080 is not one the host forwards"
        );
        let both = within_ceiling(&ranged, JobNetwork::Open, &[1]).unwrap_err();
        assert_eq!(
            both,
            BeyondCeiling::Network {
                asked: JobNetwork::Open,
                ceiling: JobNetwork::AllowList,
            }
        );
        for (port, inside) in [(7999, false), (8000, true), (8010, true), (8011, false)] {
            let verdict = within_ceiling(&ranged, JobNetwork::AllowList, &[port]);
            assert_eq!(verdict.is_ok(), inside, "{port}");
        }
        assert_eq!(within_ceiling(&none, JobNetwork::None, &[]), Ok(()));
        assert_eq!(within_ceiling(&ranged, JobNetwork::AllowList, &[]), Ok(()));
    }
}
