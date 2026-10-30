//! Which registries and namespaces a job's image may come from (`--container-pull-allow`).
//!
//! With `--container-pull missing` the host's engine pulls the image during `create`, so a
//! job could name any registry the host reaches. The wire layer already refuses an image
//! that is not pinned by digest; this list narrows where the digest may be fetched from.
//! The entries are [`js_oci::Allow`] entries (`HOST[:PORT]/NAMESPACE-PREFIX`) and a
//! reference is read with [`js_oci::Location`], so the agent and the registry client agree
//! on what a name means: a one-component Docker Hub name is under `library/`, host names are compared in lower case, an omitted port is 443, and a
//! prefix covers whole path components only.
//!
//! An empty list admits every image: it is the behaviour of an agent without the flag.
//!
//! When the list is not empty, only a registry-qualified name is admitted: the first path
//! component must hold a `.` or a `:`, or be `localhost`, and a name has a `/`. So `x`,
//! `library/x` and `ubi8/ubi` are refused and `docker.io/library/x` is admitted if listed.
//! The reason is that the engine resolves a short name through its short-name aliases
//! (`registries.conf.d`) and search registries, to a registry this check never sees.
//!
//! The name as written in the job is what is judged. The engine's `registries.conf`
//! rewrites and mirrors still apply after this check, so the host's `policy.json` is the
//! guard on the reference that is finally pulled.

use std::fmt;

use js_oci::{Allow, Location};

/// The most entries `--container-pull-allow` takes.
pub const MAX_ENTRIES: usize = 64;

/// The checked `--container-pull-allow` entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullAllow(Vec<Allow>);

/// Why an entry of `--container-pull-allow` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    /// More than [`MAX_ENTRIES`] entries.
    Count,
    /// The entry is not `HOST[:PORT]/NAMESPACE-PREFIX`, or its host is not spelt as a
    /// registry (a `.` or a `:`, or `localhost`): `komira-ai/kbf` would be read as host
    /// `komira-ai` and never match; holds the entry.
    Entry(String),
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count => write!(f, "at most {MAX_ENTRIES} entries"),
            Self::Entry(entry) => write!(
                f,
                "{entry:?}: not HOST[:PORT]/NAMESPACE-PREFIX (such as ghcr.io/komira-ai/); \
                 a bare host is refused"
            ),
        }
    }
}

/// Why an image is outside the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outside {
    /// The name does not parse as a registry path (an upper-case path, an empty
    /// component, a malformed registry); an engine would not pull it either.
    Unreadable,
    /// The name is not registry-qualified (`x`, `library/x`, `ubi8/ubi`), so the engine
    /// may resolve it to a registry this check never sees.
    Unqualified,
    /// The name parses and no entry covers it; holds the registry and repository as a pull
    /// would see them.
    NotListed(String),
}

impl fmt::Display for Outside {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable => f.write_str("the image name is not a registry path"),
            Self::Unqualified => f.write_str(
                "the image name is not registry-qualified (write docker.io/... for Docker Hub), \
                 and this agent has a --container-pull-allow list",
            ),
            Self::NotListed(at) => write!(
                f,
                "the image {at} is under no --container-pull-allow entry of this agent"
            ),
        }
    }
}

impl PullAllow {
    /// Checks the entries.
    ///
    /// # Errors
    ///
    /// [`EntryError`] for the first entry at fault, or for too many.
    pub fn parse(entries: &[String]) -> Result<Self, EntryError> {
        if entries.len() > MAX_ENTRIES {
            return Err(EntryError::Count);
        }
        entries
            .iter()
            .map(|entry| {
                let bad = || EntryError::Entry(entry.clone());
                let host = entry.split_once('/').map_or("", |(host, _)| host);
                if !spells_registry(host) {
                    return Err(bad());
                }
                Allow::parse(entry).map_err(|_| bad())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// Whether there is no entry, so that every image is admitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a job may name `image` (`name[:tag][@digest]`): always when the list is
    /// empty, else when an entry covers its registry and repository.
    ///
    /// # Errors
    ///
    /// [`Outside`] when it is not covered.
    pub fn check(&self, image: &str) -> Result<(), Outside> {
        if self.0.is_empty() {
            return Ok(());
        }
        let name = name_of(image);
        if !name
            .split_once('/')
            .is_some_and(|(first, _)| spells_registry(first))
        {
            return Err(Outside::Unqualified);
        }
        let location = Location::parse(name).map_err(|_| Outside::Unreadable)?;
        match self.0.iter().any(|allow| allow.admits(&location)) {
            true => Ok(()),
            false => Err(Outside::NotListed(format!(
                "{}/{}",
                location.registry(),
                location.repository()
            ))),
        }
    }
}

/// The repository name of `image`: the digest and the tag cut off. The tag follows a `:`
/// after the last `/`, so the `:` of a registry port is not one.
fn name_of(image: &str) -> &str {
    let rest = image.split_once('@').map_or(image, |(rest, _)| rest);
    let start = rest.rfind('/').map_or(0, |slash| slash + 1);
    match rest[start..].find(':') {
        Some(colon) => &rest[..start + colon],
        None => rest,
    }
}

/// Whether `first` is spelt as a registry: it holds a `.` or a `:`, or is `localhost`
/// (the rule an engine uses to tell a registry from the first path component).
fn spells_registry(first: &str) -> bool {
    first.contains(['.', ':']) || first == "localhost"
}
