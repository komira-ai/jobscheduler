//! `release hostkeeper-index --dir <dir> --version X.Y.Z`: the release index of the hostkeeper
//! packages in `<dir>`, written as `<dir>/js-agent-index.json`.
//!
//! The index is the file hostkeeper reads to find the package of its platform and to check what
//! it downloaded: for each platform the package's file name, SHA-256 and size. It is written in
//! hostkeeper's canonical form (`hostkeeper_config::index::ReleaseIndex::to_json`): keys sorted,
//! packages in the order of the platforms' spellings, no white space and no newline at the end;
//! `tests/hostkeeper_package` reads it back with hostkeeper's own parser and checks that the
//! parser writes the same text. `<dir>` must hold exactly the three packages [`file_name`] names
//! for the version, one per platform; any other file whose name ends in `.tar.zst` (a package of
//! another version, of another platform, or under another name) is refused, and so is a missing
//! package. Files of other kinds are left alone. The index is not signed: hostkeeper trusts the
//! packages' attestations.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::hostkeeper_package::{Platform, SERVICE, file_name, write_new};
use super::{Error, Version, io_error};

/// The name of the index inside the directory.
pub const INDEX_FILE: &str = "js-agent-index.json";

/// The suffix of a package.
const SUFFIX: &str = ".tar.zst";

/// The arguments of `release hostkeeper-index`.
#[derive(clap::Args)]
pub struct IndexArgs {
    /// The directory that holds the three packages; the index is written there.
    #[arg(long)]
    pub dir: PathBuf,
    /// The release version, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: String,
}

/// The names of the files of `dir` that end in [`SUFFIX`], sorted.
fn package_names(dir: &Path) -> Result<Vec<String>, Error> {
    let listed = std::fs::read_dir(dir).and_then(|entries| {
        entries
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<std::io::Result<Vec<_>>>()
    });
    let mut names: Vec<String> = listed
        .map_err(|e| io_error(dir, &e))?
        .iter()
        .filter_map(|name| name.to_str())
        .filter(|name| name.ends_with(SUFFIX))
        .map(str::to_owned)
        .collect();
    names.sort_unstable();
    Ok(names)
}

/// Runs `release hostkeeper-index`: the line to print.
///
/// # Errors
/// [`Error::NotAVersion`]; [`Error::Io`] for the directory, a package or the index (an index
/// that exists is refused); [`Error::Packages`] when the packages in `dir` are not exactly the
/// three of the version.
pub fn run(a: &IndexArgs) -> Result<String, Error> {
    let version: Version = a.version.parse()?;
    let found = package_names(&a.dir)?;
    let wanted: Vec<(Platform, String)> = Platform::ALL
        .iter()
        .map(|platform| (*platform, file_name(version, *platform)))
        .collect();
    if let Some(problem) = problem(&found, &wanted) {
        return Err(Error::Packages(problem));
    }
    let mut entries = Vec::new();
    for (platform, name) in &wanted {
        let path = a.dir.join(name);
        let bytes = std::fs::read(&path).map_err(|e| io_error(&path, &e))?;
        entries.push(entry(*platform, name, &bytes));
    }
    let json = format!(
        "{{\"packages\":[{}],\"schema\":1,\"service\":\"{SERVICE}\",\"version\":\"{version}\"}}",
        entries.join(",")
    );
    write_new(&a.dir.join(INDEX_FILE), json.as_bytes())?;
    Ok(format!("{INDEX_FILE}: {} packages", wanted.len()))
}

/// What is wrong with the `found` package names against the `wanted` ones, if anything: the
/// missing ones and the unexpected ones, by name.
fn problem(found: &[String], wanted: &[(Platform, String)]) -> Option<String> {
    let missing: Vec<&str> = wanted
        .iter()
        .map(|(_, name)| name.as_str())
        .filter(|name| !found.iter().any(|f| f == name))
        .collect();
    let unexpected: Vec<&str> = found
        .iter()
        .map(String::as_str)
        .filter(|name| !wanted.iter().any(|(_, w)| w == name))
        .collect();
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing {}", missing.join(", ")));
    }
    if !unexpected.is_empty() {
        parts.push(format!("unexpected {}", unexpected.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The index entry of the package `file` of `platform` whose bytes are `bytes`: an object with
/// its keys in order. Every value is plain ASCII with nothing to escape.
fn entry(platform: Platform, file: &str, bytes: &[u8]) -> String {
    format!(
        "{{\"bytes\":{},\"file\":\"{file}\",\"platform\":\"{}\",\"sha256\":\"{}\"}}",
        bytes.len(),
        platform.as_str(),
        hex::encode(Sha256::digest(bytes))
    )
}

#[cfg(test)]
mod tests;
