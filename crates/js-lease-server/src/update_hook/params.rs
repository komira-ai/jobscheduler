//! The `--params <file>` table.
//!
//! The updater writes the declaration's `[hook.params]` table to a file, a flat TOML table of
//! scalars, and adds its reserved keys. This hook reads:
//!
//! - `updater.staged_release`: the verified staged release directory, an absolute path;
//!   required;
//! - `updater.previous_release`: the release directory that runs now, an absolute path;
//!   absent on a first install;
//! - `updater.target_version`: the version being installed; required;
//! - `updater.previous_version`: the version that runs now; absent on a first install;
//! - `defer_retry_secs`, the declaration's own: what a deferred `prepare` asks the
//!   coordinator to wait before ordering the host again, whole seconds, zero allowed;
//!   required.
//!
//! Any other key is ignored: the updater adds keys. The reserved keys contain a dot, so in the
//! file they are quoted keys. The release link and the unit are not keys: they come as flags
//! before the step ([`Flags`]), put in by [`Params::with_flags`].

use std::io;
use std::path::{Path, PathBuf};

use super::args::Flags;

/// The reserved key: the verified staged release directory.
pub const STAGED_RELEASE_KEY: &str = "updater.staged_release";
/// The reserved key: the release directory that runs now.
pub const PREVIOUS_RELEASE_KEY: &str = "updater.previous_release";
/// The reserved key: the version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";
/// The reserved key: the version that runs now.
pub const PREVIOUS_VERSION_KEY: &str = "updater.previous_version";
/// The key of the time a deferred `prepare` asks the coordinator to wait.
pub const DEFER_RETRY_KEY: &str = "defer_retry_secs";

/// Why a params file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    /// The file could not be read.
    #[error("cannot read the params file: {0}")]
    Read(#[from] io::Error),
    /// The file is not a TOML table.
    #[error("the params file is not a TOML table: {0}")]
    Syntax(String),
    /// A key this hook needs is absent.
    #[error("params key {0:?} is required")]
    Missing(&'static str),
    /// A key this hook reads as text is not a string.
    #[error("params key {0:?} must be a string")]
    NotText(&'static str),
    /// A key that names a directory is not an absolute path.
    #[error("params key {0:?} must be an absolute path")]
    Relative(&'static str),
    /// A key this hook reads is not a whole, non-negative number of seconds.
    #[error("params key {0:?} must be a whole number of seconds, not negative")]
    NotSeconds(&'static str),
}

/// What the hook reads of its params file, with the flags before the step put in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The verified staged release directory (`updater.staged_release`).
    pub staged_release: PathBuf,
    /// The release directory that runs now (`updater.previous_release`); `None` on a first
    /// install.
    pub previous_release: Option<PathBuf>,
    /// The version being installed (`updater.target_version`).
    pub target_version: String,
    /// The version that runs now (`updater.previous_version`); `None` on a first install.
    pub previous_version: Option<String>,
    /// What a deferred `prepare` asks the coordinator to wait (`defer_retry_secs`).
    pub defer_retry_secs: u64,
    /// The release link (`--release-link`), when the flags name it.
    pub release_link: Option<PathBuf>,
    /// The unit (`--unit`, or its default).
    pub unit: String,
}

impl Params {
    /// Reads a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the file cannot be read or parsed, or a value is wrong.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        Params::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a params file. The flags' values are not in it yet:
    /// [`Params::with_flags`] puts them in.
    ///
    /// # Errors
    /// [`ParamsError`]: the text is not a table with the required keys.
    pub fn parse(text: &str) -> Result<Params, ParamsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ParamsError::Syntax(e.message().to_owned()))?;
        let defer = table.get(DEFER_RETRY_KEY);
        let defer_retry_secs = defer
            .ok_or(ParamsError::Missing(DEFER_RETRY_KEY))?
            .as_integer()
            .and_then(|n| u64::try_from(n).ok())
            .ok_or(ParamsError::NotSeconds(DEFER_RETRY_KEY))?;
        Ok(Params {
            staged_release: path(&table, STAGED_RELEASE_KEY)?
                .ok_or(ParamsError::Missing(STAGED_RELEASE_KEY))?,
            previous_release: path(&table, PREVIOUS_RELEASE_KEY)?,
            target_version: text_of(&table, TARGET_VERSION_KEY)?
                .ok_or(ParamsError::Missing(TARGET_VERSION_KEY))?,
            previous_version: text_of(&table, PREVIOUS_VERSION_KEY)?,
            defer_retry_secs,
            release_link: None,
            unit: Flags::default().unit().to_owned(),
        })
    }

    /// The params with the flags before the step put in: the release link, and the unit
    /// (its default when the flag is absent).
    #[must_use]
    pub fn with_flags(mut self, flags: &Flags) -> Params {
        self.release_link.clone_from(&flags.release_link);
        flags.unit().clone_into(&mut self.unit);
        self
    }
}

/// `key` as a string; `None` when it is absent.
fn text_of(table: &toml::Table, key: &'static str) -> Result<Option<String>, ParamsError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(ParamsError::NotText(key))
        })
        .transpose()
}

/// `key` as an absolute path; `None` when it is absent.
fn path(table: &toml::Table, key: &'static str) -> Result<Option<PathBuf>, ParamsError> {
    text_of(table, key)?
        .map(|text| {
            let path = PathBuf::from(text);
            match path.is_absolute() {
                true => Ok(path),
                false => Err(ParamsError::Relative(key)),
            }
        })
        .transpose()
}
