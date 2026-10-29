use std::collections::BTreeMap;
use std::path::PathBuf;

use js_types::{HostId, IdError, MemoryPercent, MemoryPercentError, Resources, Shape, TypeError};
use serde::Deserialize;

/// The example configuration shipped with the crate, shown in the crate docs.
pub const EXAMPLE_CONFIG: &str = include_str!("../example.toml");

/// The pool's configuration was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The path that was given.
        path: PathBuf,
        /// Why the read failed.
        #[source]
        source: std::io::Error,
    },
    /// The text is not valid configuration. The message names the line.
    #[error("{0}")]
    Parse(String),
    /// A host id is malformed.
    #[error(transparent)]
    Id(#[from] IdError),
    /// A host's resources are malformed.
    #[error("host {host}: {source}")]
    Resources {
        /// The host whose resources were refused.
        host: HostId,
        /// What was wrong.
        #[source]
        source: TypeError,
    },
    /// Two hosts share an id.
    #[error("host {0} is listed twice")]
    Duplicate(HostId),
    /// The file lists no host.
    #[error("the pool lists no host")]
    NoHosts,
    /// `memory_schedulable_percent` is not from 1 to 100.
    #[error(transparent)]
    MemoryPercent(#[from] MemoryPercentError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    memory_schedulable_percent: Option<u64>,
    #[serde(default)]
    host: Vec<RawHost>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    id: String,
    gpu_class: Option<String>,
    resources: BTreeMap<String, u64>,
    #[serde(default)]
    capabilities: BTreeMap<String, String>,
}

/// What a hosts file lists: the hosts, as the operator wrote them, and the share of their
/// memory the file books.
pub(crate) struct Parsed {
    /// The hosts in file order, with the memory as written.
    pub(crate) hosts: Vec<(HostId, Shape)>,
    /// `memory_schedulable_percent`, or all of it when the file leaves it out.
    pub(crate) memory_percent: MemoryPercent,
}

/// Reads configuration text into the hosts it lists, in file order.
pub(crate) fn parse(source: &str) -> Result<Parsed, ConfigError> {
    let raw: RawConfig =
        toml::from_str(source).map_err(|err| ConfigError::Parse(err.to_string()))?;
    let memory_percent = match raw.memory_schedulable_percent {
        Some(percent) => MemoryPercent::new(percent)?,
        None => MemoryPercent::FULL,
    };
    let mut hosts: Vec<(HostId, Shape)> = Vec::new();
    for entry in raw.host {
        let id = HostId::new(&entry.id)?;
        let dims = entry
            .resources
            .iter()
            .map(|(name, count)| (name.as_str(), *count));
        let resources = Resources::from_named(dims, entry.gpu_class).map_err(|source| {
            ConfigError::Resources {
                host: id.clone(),
                source,
            }
        })?;
        if hosts.iter().any(|(seen, _)| seen == &id) {
            return Err(ConfigError::Duplicate(id));
        }
        hosts.push((id, Shape::new(resources, entry.capabilities)));
    }
    if hosts.is_empty() {
        return Err(ConfigError::NoHosts);
    }
    Ok(Parsed {
        hosts,
        memory_percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "[[host]]\nid = \"a\"\n[host.resources]\ncpu_millis = 1\n";

    /// Asserts that the shipped example parses into its two hosts with their shapes.
    #[test]
    fn example_config_parses() {
        let hosts = parse(EXAMPLE_CONFIG).unwrap().hosts;
        let ids: Vec<_> = hosts.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["gpu-01", "gpu-02"]);
        let shape = &hosts[0].1;
        assert_eq!(shape.resources().gpus(), 8);
        assert_eq!(shape.resources().gpu_class(), Some("nvidia-a100"));
        assert_eq!(
            shape.capabilities().get("driver").map(String::as_str),
            Some("550")
        );
    }

    /// Asserts that a syntax error names its line.
    #[test]
    fn syntax_error_names_the_line() {
        let err = parse("[[host]]\nid = \"a\"\nresources = = 3\n")
            .err()
            .unwrap();
        assert!(matches!(err, ConfigError::Parse(_)));
        assert!(err.to_string().contains("line 3"), "{err}");
    }

    /// Asserts that a type error and an unknown key both name their line, so a typo
    /// does not pass as a default.
    #[test]
    fn type_error_and_unknown_key_name_their_line() {
        let err = parse("[[host]]\nid = 7\n").err().unwrap();
        assert!(err.to_string().contains("line 2"), "{err}");
        let err = parse("[[host]]\nid = \"a\"\nspeed = 1\n[host.resources]\n")
            .err()
            .unwrap();
        assert!(err.to_string().contains("line 3"), "{err}");
        assert!(err.to_string().contains("speed"), "{err}");
    }

    /// Asserts that a malformed host id is refused.
    #[test]
    fn bad_host_id_is_refused() {
        let err = parse("[[host]]\nid = \"Gpu One\"\n[host.resources]\n")
            .err()
            .unwrap();
        assert!(matches!(err, ConfigError::Id(_)));
    }

    /// Asserts that an unknown dimension is refused by name, with the host.
    #[test]
    fn unknown_dimension_is_refused_by_name() {
        let err = parse("[[host]]\nid = \"a\"\n[host.resources]\nvms = 2\n")
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "host a: unknown resource dimension \"vms\""
        );
    }

    /// Asserts that gpus without a gpu_class is refused.
    #[test]
    fn gpus_without_a_class_are_refused() {
        let err = parse("[[host]]\nid = \"a\"\n[host.resources]\ngpus = 1\n")
            .err()
            .unwrap();
        assert!(matches!(err, ConfigError::Resources { .. }));
    }

    /// Asserts that a repeated host id is refused and an empty file is refused.
    #[test]
    fn duplicate_and_empty_are_refused() {
        let twice = format!("{ONE}{ONE}");
        assert_eq!(
            parse(&twice).err().unwrap().to_string(),
            "host a is listed twice"
        );
        assert!(matches!(parse(""), Err(ConfigError::NoHosts)));
        assert!(parse(ONE).is_ok());
    }
}
