//! `release next-version --version X.Y.Z [--output <file>]`: the version after a package's,
//! `MAJOR.MINOR.(PATCH+1)`. The macOS package workflow builds the update package at it
//! (`js-agent-macos-pkg.yml`'s `build-update`) and the first-install workflow tells its test
//! which version it is (`js-agent-macos-first-install.yml`), through the local action
//! `.github/actions/js-agent-next-version`, so the two cannot name different versions.
//!
//! The version is checked first: it must be `MAJOR.MINOR.PATCH` of plain decimal numbers
//! (no leading zero, no sign, no space), and a version that is not is refused by name. A patch
//! that cannot be raised (the largest `u64`) is refused too, not wrapped. The verb prints
//! `version=<next>` and, with `--output` (on Actions, `GITHUB_OUTPUT`), appends that line to
//! the file.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{Error, Version, io_error};

/// The arguments of `release next-version`.
#[derive(clap::Args)]
pub struct NextVersionArgs {
    /// The version, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: String,
    /// The file the `version=<next>` line is appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// The version after `version`: its patch plus one.
pub fn after(version: &str) -> Result<Version, Error> {
    let given: Version = version.parse()?;
    let patch = given
        .patch
        .checked_add(1)
        .ok_or_else(|| Error::PatchOverflow(version.to_owned()))?;
    Ok(Version { patch, ..given })
}

/// The `release next-version` verb: the line to print.
pub fn run(a: &NextVersionArgs) -> Result<String, Error> {
    let line = format!("version={}", after(&a.version)?);
    if let Some(path) = &a.output {
        append(path, &line)?;
    }
    Ok(line)
}

/// Appends `line` to the file `path`, making it if needed.
fn append(path: &Path, line: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"))
        .map_err(|e| io_error(path, &e))
}
