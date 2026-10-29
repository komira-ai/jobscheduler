//! The config document (format section 1.3) and the closed vocabularies it uses.
//!
//! Field order in each struct is the order of the canonical JSON; reordering a field
//! changes every config digest, so the golden test pins it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::names::{DiffId, TreeName};
use crate::template::Template;
use crate::validate::{self, MAX_CONFIG_BYTES};
use crate::version::DottedVersion;

/// The config of a bundle, `application/vnd.komira.js.bundle.config.v1+json`.
///
/// Unknown fields are refused, so a writer of a newer schema is refused loudly rather
/// than half read. Read a config with [`BundleConfig::parse`]; a config built in
/// memory is checked with [`BundleConfig::validate`] before it is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleConfig {
    /// The schema major version, `1`.
    pub schema: u32,
    /// The operating system, `darwin`.
    pub os: String,
    /// The CPU architecture, `arm64`.
    pub architecture: String,
    /// The minimum macOS version the bundle runs on.
    #[serde(rename = "os.version")]
    pub os_version: DottedVersion,
    /// The bundle's name.
    pub name: String,
    /// The bundle's version.
    pub version: String,
    /// One entry per manifest layer, by index.
    pub layers: Vec<Layer>,
    /// The environment rules, applied in order.
    pub env: Vec<EnvRule>,
    /// Whether the system directories follow the bundle's `PATH` entries.
    pub system_path: bool,
    /// Programs a job names as `@bundle/<name>`.
    pub entrypoints: BTreeMap<String, Entrypoint>,
    /// Trees that need a writable clone because their tools write beside themselves.
    pub writable_trees: Vec<TreeName>,
    /// What the tools need from the host's file system and network.
    pub seatbelt: Seatbelt,
    /// What the tools need from a host Xcode; absent means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xcode: Option<XcodeNeed>,
    /// How the bundle was built.
    pub built: Built,
}

/// One tree of a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// The tree's name.
    pub tree: TreeName,
    /// The digest of the uncompressed tar, which names the tree in the store.
    pub diff_id: DiffId,
    /// The ceiling on the tree's bytes: a decompression-bomb guard.
    pub uncompressed_bytes: u64,
    /// The ceiling on the tree's entries.
    pub entries: u64,
    /// An SPDX licence expression.
    pub license: String,
    /// Where the tree's source is.
    pub source_url: String,
}

/// How an [`EnvRule`] combines with a variable that may already be set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvOp {
    /// Replaces the variable; one value.
    Set,
    /// Puts the values, joined by `:`, before the existing value.
    Prepend,
    /// Puts the values, joined by `:`, after the existing value.
    Append,
    /// Sets the variable only when it is absent; one value.
    Default,
}

/// One environment rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvRule {
    /// The variable, as [`crate::check_env_name`] allows.
    pub name: String,
    /// How the rule combines.
    pub op: EnvOp,
    /// The values: exactly one for `set` and `default`, at least one for `prepend`
    /// and `append`.
    pub values: Vec<Template>,
    /// Whether a job's environment is refused when it sets the variable.
    pub locked: bool,
}

/// A program of the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entrypoint {
    /// `${tree:NAME}/relative/path` to a regular file of the tree.
    pub path: Template,
}

/// A read a bundle may ask the agent for: the closed vocabulary. The agent maps each
/// to a fixed set of paths or a sysctl class; a bundle names no path, no pattern and
/// no profile text. A new need is a new value here, in an agent release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadGrantKind {
    /// Certificate stores.
    SystemCerts,
    /// The time zone database.
    Timezone,
    /// The resolver configuration.
    ResolverConfig,
    /// The sysctl information class (no file).
    SysctlInfo,
    /// Locale data.
    Locale,
}

impl ReadGrantKind {
    /// Every kind, in the order of the format.
    pub const ALL: [ReadGrantKind; 5] = [
        ReadGrantKind::SystemCerts,
        ReadGrantKind::Timezone,
        ReadGrantKind::ResolverConfig,
        ReadGrantKind::SysctlInfo,
        ReadGrantKind::Locale,
    ];

    /// The name in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            ReadGrantKind::SystemCerts => "system-certs",
            ReadGrantKind::Timezone => "timezone",
            ReadGrantKind::ResolverConfig => "resolver-config",
            ReadGrantKind::SysctlInfo => "sysctl-info",
            ReadGrantKind::Locale => "locale",
        }
    }
}

/// How much network a run may use, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkLevel {
    /// No network.
    Off,
    /// Loopback only.
    Loopback,
    /// Unrestricted.
    On,
}

/// The `seatbelt` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seatbelt {
    /// The reads the tools need, without repeats.
    pub reads: Vec<ReadGrantKind>,
    /// The highest network level the tools need. The agent clamps a job's request to
    /// the lower of this and its own limit (`Ord` is the order: `Off` is lowest).
    pub network_max: NetworkLevel,
}

/// A component of Xcode a bundle's tools call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XcodeComponent {
    /// The C compiler.
    Clang,
    /// The Swift compiler.
    Swift,
    /// The Metal compiler.
    Metal,
    /// The simulator control tool.
    Simctl,
}

/// A constraint on the host Xcode, not content: the bundle ships none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XcodeNeed {
    /// The lowest Xcode version that works.
    pub min: DottedVersion,
    /// The components the tools call, without repeats.
    pub components: Vec<XcodeComponent>,
}

impl XcodeNeed {
    /// Whether an Xcode of `version` that has the components `have` meets the need.
    pub fn satisfied_by(&self, version: &DottedVersion, have: &[XcodeComponent]) -> bool {
        if *version < self.min {
            return false;
        }
        self.components.iter().all(|c| have.contains(c))
    }
}

/// The `built` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Built {
    /// The deployment target the binaries were built for.
    pub deployment_target: DottedVersion,
    /// The tool that wrote the bundle, with its version.
    pub tool: String,
}

impl BundleConfig {
    /// Reads a config: refuses a document over [`MAX_CONFIG_BYTES`] unread, parses it
    /// (refusing unknown fields and repeated keys of the structs), and validates it.
    pub fn parse(bytes: &[u8]) -> Result<BundleConfig, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge {
                len: bytes.len(),
                max: MAX_CONFIG_BYTES,
            });
        }
        let config: BundleConfig =
            serde_json::from_slice(bytes).map_err(|err| ConfigError::Json(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks every rule of format section 1.3 that the types do not: a pure function
    /// of the config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate::validate(self)
    }

    /// The canonical JSON: fields in declaration order, entrypoints sorted by name, no
    /// whitespace, no trailing newline. Two equal configs give equal bytes, so this
    /// is what a pack tool hashes. A config that parsed from other spellings (a
    /// version `14.0.0`, other whitespace or order) writes the normalised form.
    ///
    /// Writing these types as JSON cannot fail; were it to, the result is empty, which
    /// no digest check accepts.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}
