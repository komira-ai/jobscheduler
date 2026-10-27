//! `release stamp --to X.Y.Z [--repo-dir <dir>]`: sets the release version in a CI
//! checkout, so the binaries built from it print it with `--version`. The version in the
//! repository is not changed: the checkout is thrown away with the job.
//!
//! Two files change, nothing else:
//!
//! - the root `Cargo.toml`: the `version` line under `[workspace.package]`, which every
//!   workspace crate inherits;
//! - `Cargo.lock`: the version of each package that has no `source` (the workspace
//!   members), so `cargo build --locked` still accepts the lock file.
//!
//! Stamping the version a checkout already has changes nothing; stamping back the version
//! it had undoes a stamp.

use std::path::{Path, PathBuf};

use super::{Error, Version, read, workspace_version, write};

/// The arguments of `release stamp`.
#[derive(clap::Args)]
pub struct StampArgs {
    /// The version to set, `MAJOR.MINOR.PATCH` (`release plan`'s `version`).
    #[arg(long)]
    pub to: Version,
    /// The checkout to stamp.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
}

/// `manifest` with the `[workspace.package]` version set to `version`.
pub fn set_manifest_version(manifest: &str, version: Version) -> Result<String, Error> {
    let mut in_section = false;
    let mut done = false;
    let mut out = String::with_capacity(manifest.len());
    for line in manifest.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[workspace.package]";
        }
        let is_version = trimmed
            .strip_prefix("version")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if in_section && !done && is_version {
            out.push_str(&format!("version = \"{version}\"\n"));
            done = true;
        } else {
            out.push_str(line);
        }
    }
    if done {
        Ok(out)
    } else {
        Err(Error::NoVersionLine)
    }
}

/// `lock` with the version of every package that has no `source` (a workspace member)
/// changed from `old` to `new`. A package with a `source` keeps its version, whatever it
/// is.
pub fn set_lock_version(lock: &str, old: Version, new: Version) -> String {
    let from = format!("\nversion = \"{old}\"\n");
    let to = format!("\nversion = \"{new}\"\n");
    lock.split("[[package]]")
        .map(|package| {
            if package.contains("\nsource = ") {
                package.to_owned()
            } else {
                package.replacen(&from, &to, 1)
            }
        })
        .collect::<Vec<_>>()
        .join("[[package]]")
}

/// The `release stamp` verb: the line to print.
pub fn run(a: &StampArgs) -> Result<String, Error> {
    stamp(&a.repo_dir, a.to)
}

/// Stamps the checkout at `dir` with `to`.
fn stamp(dir: &Path, to: Version) -> Result<String, Error> {
    let manifest_path = dir.join("Cargo.toml");
    let lock_path = dir.join("Cargo.lock");
    let manifest = read(&manifest_path)?;
    let lock = read(&lock_path)?;
    let old = workspace_version(&manifest)?;
    let manifest = set_manifest_version(&manifest, to)?;
    write(&manifest_path, &manifest)?;
    write(&lock_path, &set_lock_version(&lock, old, to))?;
    Ok(format!(
        "stamped Cargo.toml and Cargo.lock in {}: {old} -> {to}",
        dir.display()
    ))
}
