//! The `--params <file>` table of `drain` and `resume`.
//!
//! The updater writes these steps a params file of their own: the package manifest's
//! `[params]` table, the same as an update's, with the keys it adds for a maintenance run
//! (hostkeeper's `hostkeeper-hook`: `updater.service`, `updater.run_id` and, when the run sets
//! one, [`DRAIN_DEADLINE_KEY`]). It names no staged release, version or digest, so the table of
//! an update ([`Params`](super::Params)) cannot be read from it. These steps read what they use
//! of the manifest's keys, each held to the rule an update holds it to, and ignore every other
//! key:
//!
//! - `flags_file`, `token_file` and `state_root`: how the lease server is reached, as for an
//!   update, and the state root the hold file is in;
//! - `unit`: the agent's unit, which `resume` restarts when the host facts changed;
//! - `cordon_lease_secs`: how long a cordon lives between renewals (at least
//!   [`MIN_CORDON_LEASE_SECS`](super::MIN_CORDON_LEASE_SECS));
//! - `defer_retry_secs`: what a `drain` deferred by its deadline asks the updater to wait;
//! - [`DRAIN_DEADLINE_KEY`]: when a `drain` stops waiting, in whole seconds since the Unix
//!   epoch; absent, it waits for as long as the work takes.

use std::path::{Path, PathBuf};

use super::params::{
    CORDON_LEASE_KEY, DEFER_RETRY_KEY, FLAGS_FILE_KEY, ParamsError, STATE_ROOT_KEY, TOKEN_FILE_KEY,
    long_enough, required_path, required_seconds, seconds, table_of, unit_of,
};

/// The reserved key of the drain's deadline: a whole number of seconds since the Unix epoch.
/// Absent means no deadline.
pub const DRAIN_DEADLINE_KEY: &str = "updater.drain_deadline_unix";

/// What `drain` and `resume` read of their params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainParams {
    /// The unit that runs the agent (`unit`).
    pub unit: String,
    /// The file of the host's own agent flags (`flags_file`).
    pub flags_file: PathBuf,
    /// The join token file (`token_file`).
    pub token_file: PathBuf,
    /// The agent's state root (`state_root`), where the hold file is.
    pub state_root: PathBuf,
    /// How long a cordon lives between renewals.
    pub cordon_lease_secs: u64,
    /// What a deferred `drain` asks the updater to wait before it tries again; zero means "now".
    pub defer_retry_secs: u64,
    /// When `drain` stops waiting for the work to end ([`DRAIN_DEADLINE_KEY`]); `None`: never.
    pub drain_deadline_unix: Option<u64>,
}

impl DrainParams {
    /// Reads a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the file cannot be read or parsed, or a value is wrong.
    pub fn read(path: &Path) -> Result<DrainParams, ParamsError> {
        DrainParams::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a params file.
    ///
    /// # Errors
    /// [`ParamsError`]: the text is not a flat table of scalars with the keys these steps read,
    /// or a value is wrong.
    pub fn parse(text: &str) -> Result<DrainParams, ParamsError> {
        let table = table_of(text)?;
        let cordon_lease_secs = required_seconds(&table, CORDON_LEASE_KEY)?;
        long_enough(cordon_lease_secs)?;
        Ok(DrainParams {
            unit: unit_of(&table)?,
            flags_file: required_path(&table, FLAGS_FILE_KEY)?,
            token_file: required_path(&table, TOKEN_FILE_KEY)?,
            state_root: required_path(&table, STATE_ROOT_KEY)?,
            cordon_lease_secs,
            defer_retry_secs: required_seconds(&table, DEFER_RETRY_KEY)?,
            drain_deadline_unix: seconds(&table, DRAIN_DEADLINE_KEY)?,
        })
    }
}
