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
/// 2. any declared port, on an agent that forwards none (its ceiling is `none`, or lists no
///    host port), is [`refusal::PORTS_NOT_SUPPORTED`];
/// 3. a declared host port the ceiling does not list is [`refusal::PORT_ABOVE_CEILING`];
/// 4. any declared port of a `none` job is [`refusal::PORTS_NOT_SUPPORTED`]: a forward needs
///    the wired namespace a `none` job does not get.
///
/// A job at or under the ceiling is admitted at the level it asked for, with its ports: the
/// runtime forwards each from the host's port to the job.
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
    if spec.ports.is_empty() {
        return Ok(spec.network);
    }
    if ceiling.level.is_none() || ceiling.ports.is_empty() {
        return Err(NetworkRefusal {
            code: refusal::PORTS_NOT_SUPPORTED,
            detail: format!(
                "{} declared port(s) asked, and this agent forwards no declared port",
                spec.ports.len()
            ),
        });
    }
    if let Some(port) = spec.ports.iter().find(|port| !ceiling.forwards(port.host)) {
        return Err(NetworkRefusal {
            code: refusal::PORT_ABOVE_CEILING,
            detail: format!(
                "host port {} asked, agent ceiling forwards {}",
                port.host,
                ceiling.ports_text()
            ),
        });
    }
    if spec.network.is_none() {
        return Err(NetworkRefusal {
            code: refusal::PORTS_NOT_SUPPORTED,
            detail: format!(
                "{} declared port(s) asked for a job with network none, which has no address \
                 to forward to",
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

    /// Catches a job with ports admitted where nothing forwards them, and a port the agent
    /// does not list that is reported as merely unsupported, or admitted: a level above the
    /// ceiling is refused before its ports are looked at; on an agent that forwards no port
    /// (the ceiling `none` even with a port listed, or a wired ceiling that lists none) any
    /// port is `ports-not-supported`; an unlisted host port is `port-above-ceiling` naming
    /// the port and what is listed; a `none` job with listed ports is `ports-not-supported`
    /// (nothing wires its namespace); a wired job with listed ports is admitted at its level.
    #[test]
    fn ports_are_admitted_only_where_they_are_listed_and_forwarded() {
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
            &spec(JobNetwork::Open, &[22]),
        );
        let bare = bare.expect_err("22");
        assert_eq!(bare.code, refusal::PORTS_NOT_SUPPORTED);
        assert_eq!(
            bare.detail,
            "1 declared port(s) asked, and this agent forwards no declared port"
        );
        let unwired = admit(
            &ceiling(JobNetwork::None, &["22"]),
            &spec(JobNetwork::None, &[22, 23]),
        );
        let unwired = unwired.expect_err("none");
        assert_eq!(unwired.code, refusal::PORTS_NOT_SUPPORTED);
        assert_eq!(
            unwired.detail,
            "2 declared port(s) asked, and this agent forwards no declared port"
        );
        let quiet = admit(&listed, &spec(JobNetwork::None, &[80])).expect_err("none job");
        assert_eq!(quiet.code, refusal::PORTS_NOT_SUPPORTED);
        assert_eq!(
            quiet.detail,
            "1 declared port(s) asked for a job with network none, which has no address to \
             forward to"
        );
        for level in [JobNetwork::AllowList, JobNetwork::Open] {
            let within = admit(&listed, &spec(level, &[80, 8010]));
            assert_eq!(within, Ok(level), "{level}");
        }
        let without = admit(
            &ceiling(JobNetwork::None, &[]),
            &spec(JobNetwork::None, &[]),
        );
        assert_eq!(without, Ok(JobNetwork::None));
    }
}
