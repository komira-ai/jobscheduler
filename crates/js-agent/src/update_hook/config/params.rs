//! The `--params <file>` table of the config hook.
//!
//! The package manifest's `[params]` table is a flat table of scalars; the updater adds keys of
//! the namespace `updater.`, of which this hook reads `updater.previous_version` (absent on a
//! first install) and ignores the rest. Every key of [`KEYS`] is required and any other key is
//! refused:
//!
//! - `unit`: the systemd unit (a launchd label on a Mac) of the agent the hook restarts;
//! - `flags_file`: the host's agent flags, the file `js-agent --flags-file` reads, where the
//!   hook finds the lease server, the certificate to trust and the host id;
//! - `token_file` and `state_root`: the agent's `--token-file` and `--state-root`, where the
//!   bearer for the lease server's own-host routes is;
//! - `state_dir`: the hook's own lock and record, root 0700;
//! - `cutover_timeout_secs`, `verify_timeout_secs` and `rollback_timeout_secs`: the updater's
//!   bound on each step, which the waits of the step are built from.
//!
//! The paths are absolute and plain (no `.`, `..`, doubled or trailing slash).

use std::path::{Path, PathBuf};

use js_update_hook::{
    CUTOVER_MARGIN_SECS, ROLLBACK_MARGIN_SECS, START_WAIT_SECS, VERIFY_MARGIN_SECS, is_plain,
};

use crate::update_hook::params::{is_scalar, required_path, required_seconds, string_of};
use crate::update_hook::{
    CUTOVER_TIMEOUT_KEY, FLAGS_FILE_KEY, PREVIOUS_VERSION_KEY, ParamsError as TableError,
    ROLLBACK_TIMEOUT_KEY, STATE_DIR_KEY, STATE_ROOT_KEY, TOKEN_FILE_KEY, UNIT_KEY,
    VERIFY_TIMEOUT_KEY,
};

/// The reserved namespace of the updater: its keys are ignored.
const RESERVED_PREFIX: &str = "updater.";

/// How long `rollback` waits for the agent's hello once the unit is active, in seconds.
pub const HELLO_WAIT_SECS: u64 = 60;

/// Every key the hook reads from the package manifest's `[params]` table, and requires.
pub const KEYS: [&str; 8] = [
    UNIT_KEY,
    FLAGS_FILE_KEY,
    TOKEN_FILE_KEY,
    STATE_ROOT_KEY,
    STATE_DIR_KEY,
    CUTOVER_TIMEOUT_KEY,
    VERIFY_TIMEOUT_KEY,
    ROLLBACK_TIMEOUT_KEY,
];

/// The least `cutover_timeout_secs` allows: the wait for the start and the margin, with a
/// second for the stop. A timeout under it leaves no time to wait for the stop.
pub const CUTOVER_MIN_SECS: u64 = START_WAIT_SECS + CUTOVER_MARGIN_SECS + 1;

/// The least `verify_timeout_secs` allows: the margin and a second to look in.
pub const VERIFY_MIN_SECS: u64 = VERIFY_MARGIN_SECS + 1;

/// The least `rollback_timeout_secs` allows: the waits for the start and the hello, the margin
/// and a second for the stop.
pub const ROLLBACK_MIN_SECS: u64 = START_WAIT_SECS + HELLO_WAIT_SECS + ROLLBACK_MARGIN_SECS + 1;

/// Why a params file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    /// The table is not what the hook reads: unreadable, not TOML, not scalar, a key missing, a
    /// value of the wrong kind (the same words as the update hook's).
    #[error(transparent)]
    Table(#[from] TableError),
    /// A key that is neither one of [`KEYS`] nor in the updater's namespace.
    #[error("params key {0:?} is not one this hook reads")]
    Unknown(String),
    /// A path that is not spelt plainly.
    #[error("params key {0:?} must be an absolute path of plain components")]
    NotPlain(&'static str),
    /// `unit` is empty.
    #[error("params key {0:?} must not be empty")]
    Empty(&'static str),
    /// A bound too short for the waits of its step.
    #[error("params key {key:?} is {secs} s, and must be at least {min} s")]
    TooShort {
        /// The key.
        key: &'static str,
        /// What it says.
        secs: u64,
        /// The least it may say.
        min: u64,
    },
}

/// What the hook reads of its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The unit the hook restarts (`unit`).
    pub unit: String,
    /// The host's agent flags (`flags_file`).
    pub flags_file: PathBuf,
    /// The join token file (`token_file`).
    pub token_file: PathBuf,
    /// The agent's state root (`state_root`).
    pub state_root: PathBuf,
    /// The hook's lock and record (`state_dir`).
    pub state_dir: PathBuf,
    /// The updater's bound on `cutover` (`cutover_timeout_secs`).
    pub cutover_secs: u64,
    /// The updater's bound on `verify` (`verify_timeout_secs`).
    pub verify_secs: u64,
    /// The updater's bound on `rollback` (`rollback_timeout_secs`).
    pub rollback_secs: u64,
    /// The version that runs now (`updater.previous_version`); `None` on a first install, whose
    /// package the updater removes after `rollback`.
    pub previous_version: Option<String>,
}

impl Params {
    /// Reads a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the file cannot be read or parsed, or a value is wrong.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        let text = std::fs::read_to_string(path).map_err(TableError::Read)?;
        Params::parse(&text)
    }

    /// Reads the text of a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the text is not a flat table of scalars with the required keys and no
    /// others.
    pub fn parse(text: &str) -> Result<Params, ParamsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| TableError::Syntax(e.message().to_owned()))?;
        if let Some((key, _)) = table.iter().find(|(_, value)| !is_scalar(value)) {
            return Err(TableError::NotScalar(key.clone()).into());
        }
        if let Some(key) = table
            .keys()
            .find(|key| !KEYS.contains(&key.as_str()) && !key.starts_with(RESERVED_PREFIX))
        {
            return Err(ParamsError::Unknown(key.clone()));
        }
        let unit = string_of(&table, UNIT_KEY)?.ok_or(TableError::Missing(UNIT_KEY))?;
        if unit.is_empty() {
            return Err(ParamsError::Empty(UNIT_KEY));
        }
        Ok(Params {
            unit,
            flags_file: plain(&table, FLAGS_FILE_KEY)?,
            token_file: plain(&table, TOKEN_FILE_KEY)?,
            state_root: plain(&table, STATE_ROOT_KEY)?,
            state_dir: plain(&table, STATE_DIR_KEY)?,
            cutover_secs: bound(&table, CUTOVER_TIMEOUT_KEY, CUTOVER_MIN_SECS)?,
            verify_secs: bound(&table, VERIFY_TIMEOUT_KEY, VERIFY_MIN_SECS)?,
            rollback_secs: bound(&table, ROLLBACK_TIMEOUT_KEY, ROLLBACK_MIN_SECS)?,
            previous_version: string_of(&table, PREVIOUS_VERSION_KEY)?,
        })
    }
}

/// A required key that is an absolute, plain path.
fn plain(table: &toml::Table, key: &'static str) -> Result<PathBuf, ParamsError> {
    let path = required_path(table, key)?;
    if is_plain(&path) {
        Ok(path)
    } else {
        Err(ParamsError::NotPlain(key))
    }
}

/// A required key that is a whole number of seconds, at least `min`.
fn bound(table: &toml::Table, key: &'static str, min: u64) -> Result<u64, ParamsError> {
    let secs = required_seconds(table, key)?;
    if secs < min {
        Err(ParamsError::TooShort { key, secs, min })
    } else {
        Ok(secs)
    }
}
