//! The `--params <file>` table.
//!
//! The declaration's `[hook.params]` table is opaque to the updater: it checks that every value
//! is a scalar and writes the table to a file. The file is TOML, a flat table of scalars. The
//! updater adds one reserved key, [`PROMOTION_CEILING_KEY`]. This hook needs two keys,
//! `cordon_lease_secs` and `cordon_max_secs`, as whole numbers of seconds; any other scalar key
//! is the declaration's own business and is ignored.
//!
//! The reserved key contains a dot, so in the file it is a quoted key. Written bare, TOML makes
//! it a table `updater`, which is not a scalar and is refused.

use std::io;
use std::path::{Path, PathBuf};

use super::validate::is_digest;

/// The reserved key the updater sets from the declaration's `promotion_ceiling_secs`.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";

/// The key of the lifetime of a cordon between renewals.
pub const CORDON_LEASE_KEY: &str = "cordon_lease_secs";

/// The key of the longest a cordon may be held from its first placement.
pub const CORDON_MAX_KEY: &str = "cordon_max_secs";

/// The time between two looks at the host while a step waits for it to hold no lease.
pub const POLL_SECS: u64 = 10;

/// The shortest cordon lease: three polls, so that a renewal (due when less than half of it is
/// left) always has polls to be made in.
pub const MIN_CORDON_LEASE_SECS: u64 = 3 * POLL_SECS;

/// The reserved key: the digest of the verified artifact, `sha256:` and 64 lowercase hex
/// digits. The updater sets it for every step.
pub const ARTIFACT_DIGEST_KEY: &str = "updater.artifact_digest";

/// The key of the longest `prepare` and `rollback` wait for the host to hold no lease.
pub const DRAIN_WAIT_KEY: &str = "drain_wait_secs";

/// The key of the time a deferred `prepare` asks the coordinator to wait.
pub const DEFER_RETRY_KEY: &str = "defer_retry_secs";

/// The reserved key: the verified staged release directory, an absolute path.
pub const STAGED_RELEASE_KEY: &str = "updater.staged_release";

/// The reserved key: the release directory that runs now. Absent on a first install.
pub const PREVIOUS_RELEASE_KEY: &str = "updater.previous_release";

/// The reserved key: the version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";

/// The reserved key: the version that runs now. Absent on a first install.
pub const PREVIOUS_VERSION_KEY: &str = "updater.previous_version";

/// Why a params file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    /// The file could not be read.
    #[error("cannot read the params file: {0}")]
    Read(#[from] io::Error),
    /// The file is not TOML.
    #[error("the params file is not a TOML table: {0}")]
    Syntax(String),
    /// A value is an array, a table or a date.
    #[error("params key {0:?} is not a scalar (a dotted key must be quoted)")]
    NotScalar(String),
    /// A key this hook needs is absent.
    #[error("params key {0:?} is required")]
    Missing(&'static str),
    /// A key this hook reads as text is not a string.
    #[error("params key {0:?} must be a string")]
    NotText(String),
    /// A key that names a directory is not an absolute path.
    #[error("params key {0:?} must be an absolute path")]
    Relative(String),
    /// The artifact digest is not `sha256:` and 64 lowercase hex digits.
    #[error("params key {0:?} must be sha256: and 64 lowercase hex digits")]
    NotDigest(String),
    /// A key that must be positive is zero.
    #[error("params key {0:?} must not be zero")]
    Zero(String),
    /// The cordon's lease is shorter than three polls.
    #[error("cordon_lease_secs {lease} is shorter than {min} s, three polls")]
    LeaseTooShort {
        /// `cordon_lease_secs`.
        lease: u64,
        /// [`MIN_CORDON_LEASE_SECS`].
        min: u64,
    },
    /// The cordon's cap is shorter than the drain wait it has to cover.
    #[error("cordon_max_secs {cap} is shorter than drain_wait_secs {wait}")]
    CapBelowWait {
        /// `cordon_max_secs`.
        cap: u64,
        /// `drain_wait_secs`.
        wait: u64,
    },
    /// A key this hook reads is not a whole, non-negative number of seconds.
    #[error("params key {0:?} must be a whole number of seconds, not negative")]
    NotSeconds(String),
}

/// What the hook reads of its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// How long a cordon lives between renewals.
    pub cordon_lease_secs: u64,
    /// The most a cordon may be held, counted from its first placement.
    pub cordon_max_secs: u64,
    /// The longest the hook waits for the host to hold no lease.
    pub drain_wait_secs: u64,
    /// What a deferred `prepare` asks the coordinator to wait before ordering the host again.
    pub defer_retry_secs: u64,
    /// The digest of the verified artifact (`updater.artifact_digest`).
    pub artifact_digest: String,
    /// The updater's ceiling on a pending promotion, when it set one.
    pub promotion_ceiling_secs: Option<u64>,
    /// The verified staged release directory (`updater.staged_release`).
    pub staged_release: PathBuf,
    /// The release directory that runs now (`updater.previous_release`); `None` on a first
    /// install.
    pub previous_release: Option<PathBuf>,
    /// The version being installed (`updater.target_version`).
    pub target_version: String,
    /// The version that runs now (`updater.previous_version`); `None` on a first install.
    pub previous_version: Option<String>,
}

impl Params {
    /// Reads a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the file cannot be read or parsed, or a value is wrong.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        Params::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the text is not a flat table of scalars with the required keys.
    pub fn parse(text: &str) -> Result<Params, ParamsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ParamsError::Syntax(e.message().to_owned()))?;
        if let Some((key, _)) = table.iter().find(|(_, value)| !is_scalar(value)) {
            return Err(ParamsError::NotScalar(key.clone()));
        }
        let cordon_lease_secs = positive(&table, CORDON_LEASE_KEY)?;
        let cordon_max_secs = positive(&table, CORDON_MAX_KEY)?;
        let drain_wait_secs = positive(&table, DRAIN_WAIT_KEY)?;
        let defer_retry_secs =
            seconds(&table, DEFER_RETRY_KEY)?.ok_or(ParamsError::Missing(DEFER_RETRY_KEY))?;
        if cordon_max_secs < drain_wait_secs {
            return Err(ParamsError::CapBelowWait {
                cap: cordon_max_secs,
                wait: drain_wait_secs,
            });
        }
        let artifact_digest = string_of(&table, ARTIFACT_DIGEST_KEY)?
            .ok_or(ParamsError::Missing(ARTIFACT_DIGEST_KEY))?;
        if !is_digest(&artifact_digest) {
            return Err(ParamsError::NotDigest(ARTIFACT_DIGEST_KEY.to_owned()));
        }
        Ok(Params {
            cordon_lease_secs,
            cordon_max_secs,
            drain_wait_secs,
            defer_retry_secs,
            artifact_digest,
            promotion_ceiling_secs: seconds(&table, PROMOTION_CEILING_KEY)?,
            staged_release: path(&table, STAGED_RELEASE_KEY)?
                .ok_or(ParamsError::Missing(STAGED_RELEASE_KEY))?,
            previous_release: path(&table, PREVIOUS_RELEASE_KEY)?,
            target_version: string_of(&table, TARGET_VERSION_KEY)?
                .ok_or(ParamsError::Missing(TARGET_VERSION_KEY))?,
            previous_version: string_of(&table, PREVIOUS_VERSION_KEY)?,
        })
    }
}

/// A required key that is a whole number of seconds and not zero.
fn positive(table: &toml::Table, key: &'static str) -> Result<u64, ParamsError> {
    match seconds(table, key)?.ok_or(ParamsError::Missing(key))? {
        0 => Err(ParamsError::Zero(key.to_owned())),
        secs => Ok(secs),
    }
}

/// Whether the updater would have let the value through: a boolean, a number or a string.
fn is_scalar(value: &toml::Value) -> bool {
    matches!(
        value,
        toml::Value::Boolean(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::String(_)
    )
}

/// `key` as a whole, non-negative number of seconds; `None` when it is absent.
fn seconds(table: &toml::Table, key: &str) -> Result<Option<u64>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| ParamsError::NotSeconds(key.to_owned()))
        })
        .transpose()
}

/// `key` as a string; `None` when it is absent.
fn string_of(table: &toml::Table, key: &str) -> Result<Option<String>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ParamsError::NotText(key.to_owned()))
        })
        .transpose()
}

/// `key` as an absolute path; `None` when it is absent.
fn path(table: &toml::Table, key: &str) -> Result<Option<PathBuf>, ParamsError> {
    string_of(table, key)?
        .map(|text| {
            let path = PathBuf::from(text);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(ParamsError::Relative(key.to_owned()))
            }
        })
        .transpose()
}
