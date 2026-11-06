//! The `--params <file>` table.
//!
//! The package manifest's `[params]` table is opaque to the updater: it checks that every value
//! is a scalar and writes the table to a file. The file is TOML, a flat table of scalars. The
//! updater adds reserved keys ([`PROMOTION_CEILING_KEY`], the staged and previous release, the
//! versions and [`ARTIFACT_DIGEST_KEY`]). The keys the manifest gives are [`KEYS`]; every one is
//! required, and any other scalar key is ignored:
//!
//! - `unit`, `state_dir`, `runtime` and the five `*_timeout_secs` and `verify_deadline_secs`:
//!   the [`Settings`] (the unit the hook manages, its record's directory, the runtime the agent
//!   must have, and the updater's bound on each step);
//! - `release_link`: hostkeeper's `current` link of the package, which the hook only reads;
//! - `flags_file`: the host's own agent flags (the file `js-agent --flags-file` reads), where
//!   the hook finds the lease server, the certificate to trust and the host id;
//! - `token_file` and `state_root`: the agent's `--token-file` and `--state-root` values;
//! - `cordon_lease_secs`: how long a cordon lives between renewals; at least
//!   [`MIN_CORDON_LEASE_SECS`] (three polls) and not zero;
//! - `cordon_max_secs`: the longest a cordon is held from its first placement; not zero and
//!   not shorter than `drain_wait_secs`;
//! - `drain_wait_secs`: the longest `prepare` waits for the host to hold no lease; not zero;
//! - `defer_retry_secs`: what a deferred `prepare` asks the coordinator to wait before
//!   ordering the host again. Zero is allowed and means "retry now".
//!
//! The reserved key contains a dot, so in the file it is a quoted key. Written bare, TOML makes
//! it a table `updater`, which is not a scalar and is refused.

use std::io;
use std::path::{Path, PathBuf};

use js_update_hook::is_plain;

use super::settings::{RUNTIMES, Settings, Timeouts};
use super::validate::is_digest;

/// The reserved key the updater sets from the declaration's `promotion_ceiling_secs`.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";

/// The key of the lifetime of a cordon between renewals.
pub const CORDON_LEASE_KEY: &str = "cordon_lease_secs";

/// The key of the longest a cordon may be held from its first placement.
pub const CORDON_MAX_KEY: &str = "cordon_max_secs";

pub use js_update_hook::POLL_SECS;

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

/// The key of the release link, hostkeeper's `current` link of the package: the symbolic link
/// to the release directory that runs. The updater moves it (to the staged release before
/// `cutover`, back to the previous one before `rollback`); the hook reads it and never writes it.
pub const RELEASE_LINK_KEY: &str = "release_link";

/// The reserved key: the version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";

/// The reserved key: the version that runs now. Absent on a first install.
pub const PREVIOUS_VERSION_KEY: &str = "updater.previous_version";

/// The key of the agent's state root, as the agent's `--state-root`.
pub const STATE_ROOT_KEY: &str = "state_root";

/// The key of the file holding the host's join token, as the agent's `--token-file`.
pub const TOKEN_FILE_KEY: &str = "token_file";

/// The key of the file of the host's own agent flags, as the agent's `--flags-file`.
pub const FLAGS_FILE_KEY: &str = "flags_file";

/// The key of the unit the hook manages.
pub const UNIT_KEY: &str = "unit";

/// The key of the directory of the hook record.
pub const STATE_DIR_KEY: &str = "state_dir";

/// The key of the runtime the agent must have.
pub const RUNTIME_KEY: &str = "runtime";

/// The key of the updater's bound on `prepare`, in seconds.
pub const PREPARE_TIMEOUT_KEY: &str = "prepare_timeout_secs";

/// The key of the updater's bound on `cutover`, in seconds.
pub const CUTOVER_TIMEOUT_KEY: &str = "cutover_timeout_secs";

/// The key of the updater's bound on `verify`, in seconds.
pub const VERIFY_TIMEOUT_KEY: &str = "verify_timeout_secs";

/// The key of the updater's bound on `rollback`, in seconds.
pub const ROLLBACK_TIMEOUT_KEY: &str = "rollback_timeout_secs";

/// The key of the updater's bound on `cleanup`, in seconds.
pub const CLEANUP_TIMEOUT_KEY: &str = "cleanup_timeout_secs";

/// The key of the time the updater allows a `verify` and the `rollback` after it, in seconds.
pub const VERIFY_DEADLINE_KEY: &str = "verify_deadline_secs";

/// Every key the hook reads from the package manifest's `[params]` table, and requires. A
/// manifest gives exactly these (the reserved `updater.*` keys are the updater's).
pub const KEYS: [&str; 17] = [
    UNIT_KEY,
    STATE_DIR_KEY,
    RELEASE_LINK_KEY,
    FLAGS_FILE_KEY,
    TOKEN_FILE_KEY,
    STATE_ROOT_KEY,
    RUNTIME_KEY,
    CORDON_LEASE_KEY,
    CORDON_MAX_KEY,
    DRAIN_WAIT_KEY,
    DEFER_RETRY_KEY,
    PREPARE_TIMEOUT_KEY,
    CUTOVER_TIMEOUT_KEY,
    VERIFY_TIMEOUT_KEY,
    ROLLBACK_TIMEOUT_KEY,
    CLEANUP_TIMEOUT_KEY,
    VERIFY_DEADLINE_KEY,
];

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
    /// `state_dir` is not an absolute path of plain components (no `.`, `..`, doubled or
    /// trailing slash).
    #[error("params key {0:?} must be an absolute path of plain components")]
    NotPlain(String),
    /// `unit` is empty.
    #[error("params key {0:?} must not be empty")]
    Empty(String),
    /// `runtime` is not one of the runtimes an agent has.
    #[error("runtime {0:?} is not one of crun, docker, native")]
    UnknownRuntime(String),
    /// `verify_deadline_secs` is shorter than the `verify` bound plus the `rollback` bound: the
    /// updater would roll back a verify that is still allowed to run.
    #[error(
        "verify_deadline_secs {deadline} is less than the verify bound {verify} plus the rollback bound {rollback}"
    )]
    VerifyDeadline {
        /// `verify_deadline_secs`.
        deadline: u64,
        /// `verify_timeout_secs`.
        verify: u64,
        /// `rollback_timeout_secs`.
        rollback: u64,
    },
}

/// How the hook on a machine reaches the lease server and the agent's state: the values of the
/// agent's own flags of the same names. `state_root` and `token_file` are params keys
/// ([`Params::state_root`], [`Params::token_file`]); `server`, `ca_cert` and `host_id` come from
/// the host's flags file, which the machine reads when a step asks the lease server (a first
/// install asks none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectParams {
    /// The lease server (`--server` of the flags file).
    pub server: Option<String>,
    /// The agent's state root (`state_root`), where the host credential and the taint are.
    pub state_root: Option<PathBuf>,
    /// The join token file (`token_file`), used when no state root is there or a private one
    /// holds no credential; never in place of a state root that is there and not private.
    pub token_file: Option<PathBuf>,
    /// The certificate to trust (`--ca-cert` of the flags file).
    pub ca_cert: Option<PathBuf>,
    /// The host's id (`--host-id` of the flags file); the machine's own id when absent.
    pub host_id: Option<String>,
}

/// What the hook reads of its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The unit, the record's directory, the runtime and the bounds.
    pub settings: Settings,
    /// How long a cordon lives between renewals.
    pub cordon_lease_secs: u64,
    /// The most a cordon may be held, counted from its first placement.
    pub cordon_max_secs: u64,
    /// The longest the hook waits for the host to hold no lease.
    pub drain_wait_secs: u64,
    /// What a deferred `prepare` asks the coordinator to wait before ordering the host again;
    /// zero means "retry now".
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
    /// The release link (`release_link`), which the hook reads and never writes.
    pub release_link: PathBuf,
    /// The file of the host's own agent flags (`flags_file`).
    pub flags_file: PathBuf,
    /// The version being installed (`updater.target_version`).
    pub target_version: String,
    /// The version that runs now (`updater.previous_version`); `None` on a first install.
    pub previous_version: Option<String>,
    /// The agent's state root (`state_root`), where the host credential and the taint are.
    pub state_root: PathBuf,
    /// The join token file (`token_file`).
    pub token_file: PathBuf,
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
        let defer_retry_secs = required_seconds(&table, DEFER_RETRY_KEY)?;
        if cordon_lease_secs < MIN_CORDON_LEASE_SECS {
            return Err(ParamsError::LeaseTooShort {
                lease: cordon_lease_secs,
                min: MIN_CORDON_LEASE_SECS,
            });
        }
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
            settings: settings(&table)?,
            cordon_lease_secs,
            cordon_max_secs,
            drain_wait_secs,
            defer_retry_secs,
            artifact_digest,
            promotion_ceiling_secs: seconds(&table, PROMOTION_CEILING_KEY)?,
            staged_release: required_path(&table, STAGED_RELEASE_KEY)?,
            previous_release: path(&table, PREVIOUS_RELEASE_KEY)?,
            release_link: required_path(&table, RELEASE_LINK_KEY)?,
            flags_file: required_path(&table, FLAGS_FILE_KEY)?,
            target_version: string_of(&table, TARGET_VERSION_KEY)?
                .ok_or(ParamsError::Missing(TARGET_VERSION_KEY))?,
            previous_version: string_of(&table, PREVIOUS_VERSION_KEY)?,
            state_root: required_path(&table, STATE_ROOT_KEY)?,
            token_file: required_path(&table, TOKEN_FILE_KEY)?,
        })
    }
}

/// The unit, the record's directory, the runtime and the bounds of the table.
fn settings(table: &toml::Table) -> Result<Settings, ParamsError> {
    let state_dir = required_path(table, STATE_DIR_KEY)?;
    if !is_plain(&state_dir) {
        return Err(ParamsError::NotPlain(STATE_DIR_KEY.to_owned()));
    }
    let unit = string_of(table, UNIT_KEY)?.ok_or(ParamsError::Missing(UNIT_KEY))?;
    if unit.is_empty() {
        return Err(ParamsError::Empty(UNIT_KEY.to_owned()));
    }
    let runtime = string_of(table, RUNTIME_KEY)?.ok_or(ParamsError::Missing(RUNTIME_KEY))?;
    if !RUNTIMES.contains(&runtime.as_str()) {
        return Err(ParamsError::UnknownRuntime(runtime));
    }
    let timeouts = Timeouts {
        prepare: required_seconds(table, PREPARE_TIMEOUT_KEY)?,
        cutover: required_seconds(table, CUTOVER_TIMEOUT_KEY)?,
        verify: required_seconds(table, VERIFY_TIMEOUT_KEY)?,
        rollback: required_seconds(table, ROLLBACK_TIMEOUT_KEY)?,
        cleanup: required_seconds(table, CLEANUP_TIMEOUT_KEY)?,
    };
    // The updater rolls back a verify that has not succeeded by the deadline: it must not do
    // that to one that is still allowed to run, and the rollback needs its own bound.
    let verify_deadline_secs = required_seconds(table, VERIFY_DEADLINE_KEY)?;
    if verify_deadline_secs < timeouts.verify.saturating_add(timeouts.rollback) {
        return Err(ParamsError::VerifyDeadline {
            deadline: verify_deadline_secs,
            verify: timeouts.verify,
            rollback: timeouts.rollback,
        });
    }
    Ok(Settings {
        state_dir,
        unit,
        runtime,
        timeouts,
        verify_deadline_secs,
    })
}

/// A required key that is a whole number of seconds.
fn required_seconds(table: &toml::Table, key: &'static str) -> Result<u64, ParamsError> {
    seconds(table, key)?.ok_or(ParamsError::Missing(key))
}

/// A required key that is an absolute path.
fn required_path(table: &toml::Table, key: &'static str) -> Result<PathBuf, ParamsError> {
    path(table, key)?.ok_or(ParamsError::Missing(key))
}

/// A required key that is a whole number of seconds and not zero.
fn positive(table: &toml::Table, key: &'static str) -> Result<u64, ParamsError> {
    match required_seconds(table, key)? {
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
