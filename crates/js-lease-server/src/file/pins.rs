//! The pins and the network table of an elastic pool: the agent builds a launched host
//! downloads, and how it reaches the server.

use std::collections::BTreeMap;

use js_elastic::Artifact;

use super::Section;
use crate::error::StartupError;
use crate::file_checks::{https_url, sha256};

/// What a file with no pin for something a launched host downloads is told.
pub(super) const LIVE_NEEDS: &str = "required with launch = \"live\": the bootstrap downloads it";

/// The agent pins by architecture: each key of `table` is an architecture (lower-case
/// letters, digits and `_`) and holds a pin.
pub(super) fn read_agents(
    table: toml::Table,
    head: String,
) -> Result<BTreeMap<String, Artifact>, StartupError> {
    let mut agents = BTreeMap::new();
    for (arch, value) in table {
        let place = format!("{head}{arch}");
        let letters = arch
            .chars()
            .all(|c| c.is_ascii_lowercase() | c.is_ascii_digit() | (c == '_'));
        if !letters | arch.is_empty() {
            return Err(StartupError::at(
                place,
                "an architecture is lower-case letters, digits and '_', such as x86_64 or aarch64",
            ));
        }
        let toml::Value::Table(pin) = value else {
            return Err(StartupError::at(place, "must be a table { url, sha256 }"));
        };
        let artifact = read_artifact(pin, format!("{head}{arch}."))?;
        agents.insert(arch, artifact);
    }
    Ok(agents)
}

/// A file a launched host downloads: an `https://` `url` and its `sha256`.
pub(super) fn read_artifact(table: toml::Table, head: String) -> Result<Artifact, StartupError> {
    let mut s = Section::open(table, head);
    s.only(&["url", "sha256"])?;
    let url = s.required_string("url")?;
    if !https_url(&url) {
        return Err(s.fail("url", "must be an https:// URL: the host refuses any other"));
    }
    let hash = s.required_string("sha256")?;
    match sha256(&hash) {
        Some(sha256) => Ok(Artifact { url, sha256 }),
        None => Err(s.fail("sha256", "must be 64 lower-case hex digits")),
    }
}
