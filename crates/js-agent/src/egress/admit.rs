//! The one comparison of a job's network with the agent's ceiling (design 5.8): a job that
//! asks for more than the ceiling is refused at start with a code that names what it asked
//! for and the ceiling, and is never run with less. Every runtime's admission calls it (the
//! lifecycle with the runtime's [`Runtime::network_ceiling`](crate::lifecycle::Runtime)).

use js_agent_wire::{JobSpec, NetworkCeiling, refusal};
use js_types::JobNetwork;

/// Why a start's network is refused: the wire code it is answered with and the text that
/// stays on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRefusal {
    /// [`refusal::NETWORK_ABOVE_CEILING`], [`refusal::PORT_ABOVE_CEILING`] or
    /// [`refusal::PORTS_NOT_SUPPORTED`].
    pub code: &'static str,
    /// What was asked for and what the agent allows, for the log.
    pub detail: String,
}

/// The level `spec` runs at, when `ceiling` allows it, checked in this order:
///
/// 1. a level above the ceiling's is [`refusal::NETWORK_ABOVE_CEILING`] (`network allow-list
///    asked, agent ceiling is none`);
/// 2. a declared host port the ceiling does not list is [`refusal::PORT_ABOVE_CEILING`];
/// 3. any declared port is [`refusal::PORTS_NOT_SUPPORTED`]: the agent does not forward
///    declared ports yet.
///
/// A job at or under the ceiling, with no port, is admitted at the level it asked for.
///
/// # Errors
///
/// The first of the refusals above that applies.
pub fn admit(ceiling: &NetworkCeiling, spec: &JobSpec) -> Result<JobNetwork, NetworkRefusal> {
    if !ceiling.allows(spec.network) {
        return Err(NetworkRefusal {
            code: refusal::NETWORK_ABOVE_CEILING,
            detail: format!(
                "network {} asked, agent ceiling is {}",
                spec.network, ceiling.level
            ),
        });
    }
    if let Some(port) = spec.ports.iter().find(|port| !ceiling.forwards(port.host)) {
        let listed = match ceiling.ports.is_empty() {
            true => "no port".to_owned(),
            false => ceiling.ports_text(),
        };
        return Err(NetworkRefusal {
            code: refusal::PORT_ABOVE_CEILING,
            detail: format!(
                "host port {} asked, agent ceiling forwards {listed}",
                port.host
            ),
        });
    }
    if !spec.ports.is_empty() {
        return Err(NetworkRefusal {
            code: refusal::PORTS_NOT_SUPPORTED,
            detail: format!(
                "{} declared port(s) asked, and this agent forwards no declared port yet",
                spec.ports.len()
            ),
        });
    }
    Ok(spec.network)
}

#[cfg(test)]
mod tests {
    use js_agent_wire::{Isolation, Limits, PortRange};
    use js_types::PortMap;

    use super::*;

    fn spec(network: JobNetwork, ports: &[u16]) -> JobSpec {
        JobSpec {
            run_id: "run-1".into(),
            lease_id: "1-1".into(),
            image: String::new(),
            native: None,
            args: Vec::new(),
            limits: Limits {
                cpu_millis: 1,
                memory_bytes: 1,
                disk_bytes: None,
                pids: None,
                wall_ms: 1,
            },
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            files: Vec::new(),
            isolation: Isolation::Contained,
            labels: std::collections::BTreeMap::new(),
            heartbeat_seq: 0,
            valid_for_ms: 0,
            bundle: None,
            run: None,
            os: None,
            arch: None,
            network,
            ports: ports
                .iter()
                .map(|host| PortMap {
                    host: *host,
                    container: 80,
                })
                .collect(),
        }
    }

    fn ceiling(level: JobNetwork, ports: &[&str]) -> NetworkCeiling {
        NetworkCeiling {
            level,
            ports: ports
                .iter()
                .map(|text| PortRange::parse(text).expect("a range"))
                .collect(),
        }
    }

    /// Catches a comparison that lowers a job instead of refusing it, one that lets a level
    /// above the ceiling through, and a generic code: under each ceiling every level at or
    /// under it is admitted as asked, and every level above it is refused
    /// `network-above-ceiling` with a detail that names both the level and the ceiling.
    #[test]
    fn a_level_above_the_ceiling_is_refused_naming_both() {
        let levels = [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open];
        for top in levels {
            for asked in levels {
                let admitted = admit(&ceiling(top, &[]), &spec(asked, &[]));
                if asked <= top {
                    assert_eq!(admitted, Ok(asked), "{asked} under {top}");
                    continue;
                }
                let refused = admitted.expect_err("above");
                assert_eq!(refused.code, refusal::NETWORK_ABOVE_CEILING);
                assert_eq!(
                    refused.detail,
                    format!("network {asked} asked, agent ceiling is {top}")
                );
            }
        }
    }

    /// Catches ports that are run without a forward, and a port the agent does not list that
    /// is reported as merely unsupported: a level above the ceiling is refused before its
    /// ports are looked at; an unlisted host port is `port-above-ceiling` naming the port and
    /// what is listed (`no port` when nothing is); a listed one is `ports-not-supported`.
    #[test]
    fn ports_are_refused_above_the_ceiling_first_and_then_as_not_forwarded() {
        let none = admit(
            &ceiling(JobNetwork::None, &[]),
            &spec(JobNetwork::Open, &[80]),
        );
        assert_eq!(
            none.expect_err("above").code,
            refusal::NETWORK_ABOVE_CEILING
        );
        let listed = ceiling(JobNetwork::Open, &["80", "8000-8010"]);
        let unlisted = admit(&listed, &spec(JobNetwork::Open, &[80, 8011])).expect_err("8011");
        assert_eq!(unlisted.code, refusal::PORT_ABOVE_CEILING);
        assert_eq!(
            unlisted.detail,
            "host port 8011 asked, agent ceiling forwards 80,8000-8010"
        );
        let bare = admit(
            &ceiling(JobNetwork::Open, &[]),
            &spec(JobNetwork::None, &[22]),
        );
        let bare = bare.expect_err("22");
        assert_eq!(bare.code, refusal::PORT_ABOVE_CEILING);
        assert_eq!(
            bare.detail,
            "host port 22 asked, agent ceiling forwards no port"
        );
        let within = admit(&listed, &spec(JobNetwork::AllowList, &[80, 8010])).expect_err("ports");
        assert_eq!(within.code, refusal::PORTS_NOT_SUPPORTED);
        assert!(
            within.detail.starts_with("2 declared port(s)"),
            "{}",
            within.detail
        );
    }
}
