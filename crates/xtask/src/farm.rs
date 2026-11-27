//! The farm lane's two checks of its own (`.github/workflows/farm.yml`, docs/farm.md). The lane's
//! two buck2 builds that reach the build farm, the `buildrules` command line and then `//:ci`, are
//! not run here: each is a step of komira's buck2-farm action, which runs buck2 under a watchdog of
//! the runner's connections to the farm, runs a cut or hung run again, and fails a run that ran
//! an action on the runner. What is left to this crate reaches no farm:
//!
//! - `farm-workers --buck2 <path>` ([`workers`]), before either build:
//!   `buck2 audit config buildrules_re.linux_x86_64_properties --style json` must give the key a
//!   value that is not blank: the worker property set farm-connect wrote, which `.buckconfig`
//!   hands to buildrules. Without one buck2 has no execution platform, and this says so first.
//! - `farm-table --buck2 <path> --buildrules <path> --patch <path> --summary <path> --repo
//!   <owner/name> --run-id <id>` ([`table::ensure_fresh`]), between them: the generated build
//!   table must be fresh, checked with the `buildrules` command line the first build made. When it
//!   is stale, the job runs `buildrules sync`, writes what sync changed as a patch to `--patch`
//!   (the workflow uploads it as the artifact [`table::PATCH_ARTIFACT`]) and the stale inputs, the
//!   changed files and the one command that applies the patch to `--summary` (the job summary),
//!   and fails, so `//:ci` is not built.
//!
//! Neither stops buck2's daemon: the job's last step does, however the steps ended.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub mod table;

pub use table::Handoff;

/// The key of the worker property set buildrules reads.
pub const PROPERTIES_KEY: &str = "buildrules_re.linux_x86_64_properties";

/// What `farm-workers` prints when the key has a value.
pub const WORKERS_SET: &str =
    "buildrules_re.linux_x86_64_properties is set: buck2 has the farm's worker property set";

/// Why a check of the farm lane failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A program could not be started, or waited for.
    #[error("cannot run {program:?}: {why}")]
    Start {
        /// The program that was to run.
        program: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// `buck2 audit config` ended with a failure status.
    #[error("`buck2 audit config` failed ({status}): {stderr}")]
    Audit {
        /// How it ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// The audit gave the worker property set no value, or a blank one.
    #[error(
        "buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no [komira_re] \
         linux_x86_64_properties, so buck2 has no farm to build on"
    )]
    NoWorkers,
    /// The generated build table is not fresh, or its freshness could not be settled.
    #[error(transparent)]
    Table(#[from] table::Error),
}

/// Whether `json`, what `buck2 audit config <key> --style json` printed, gives
/// [`PROPERTIES_KEY`] a value that is not blank.
pub fn has_value(json: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json)
        .ok()
        .and_then(|config| config.get(PROPERTIES_KEY).cloned())
        .and_then(|value| value.as_str().map(|v| !v.trim().is_empty()))
        .unwrap_or(false)
}

/// Fails unless the buck2 at `buck2` gives buildrules a worker property set: [`WORKERS_SET`]
/// when it does.
///
/// # Errors
/// buck2 cannot be started, its audit fails, or it names no worker property set.
pub fn workers(buck2: &Path) -> Result<String, Error> {
    let audit = output(
        buck2,
        &["audit", "config", PROPERTIES_KEY, "--style", "json"],
    )?;
    if !audit.status.success() {
        return Err(Error::Audit {
            status: audit.status.to_string(),
            stderr: String::from_utf8_lossy(&audit.stderr).trim().to_owned(),
        });
    }
    if !has_value(&String::from_utf8_lossy(&audit.stdout)) {
        return Err(Error::NoWorkers);
    }
    Ok(WORKERS_SET.to_owned())
}

/// The error for a program that could not be started or waited for.
fn start_error(program: &Path) -> impl FnOnce(std::io::Error) -> Error {
    move |e| Error::Start {
        program: program.to_owned(),
        why: e.to_string(),
    }
}

/// `program <args>` run to its end, its standard input closed.
fn output(program: &Path, args: &[&str]) -> Result<Output, Error> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(start_error(program))
}

/// Every line `from` gives, written to `log` as it comes, and all of them.
fn tee(from: impl std::io::Read, log: &mut dyn Write) -> Vec<u8> {
    let mut lines = BufReader::new(from);
    let (mut line, mut text) = (Vec::new(), Vec::new());
    // A read that fails ends the log; the status of the program is still its own.
    while lines.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        // A log that cannot be written loses the program's words, not the verdict.
        let _ = log.write_all(&line);
        text.append(&mut line);
    }
    text
}

#[cfg(test)]
mod tests;
