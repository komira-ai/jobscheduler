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

/// The reserved key the updater sets from the declaration's `promotion_ceiling_secs`.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";

/// The key of the lifetime of a cordon between renewals.
pub const CORDON_LEASE_KEY: &str = "cordon_lease_secs";

/// The key of the longest a cordon may be held from its first placement.
pub const CORDON_MAX_KEY: &str = "cordon_max_secs";

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
        Ok(Params {
            cordon_lease_secs: seconds(&table, CORDON_LEASE_KEY)?
                .ok_or(ParamsError::Missing(CORDON_LEASE_KEY))?,
            cordon_max_secs: seconds(&table, CORDON_MAX_KEY)?
                .ok_or(ParamsError::Missing(CORDON_MAX_KEY))?,
            promotion_ceiling_secs: seconds(&table, PROMOTION_CEILING_KEY)?,
            staged_release: PathBuf::new(),
            previous_release: None,
            target_version: String::new(),
            previous_version: None,
        })
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

#[allow(dead_code)] // the next commit reads the text keys
fn text(_: &toml::Table, _: &str) -> Result<Option<String>, ParamsError> {
    Ok(None)
}

#[allow(dead_code)] // the next commit reads the path keys
fn path(_: &toml::Table, _: &str) -> Result<Option<PathBuf>, ParamsError> {
    Ok(None)
}
