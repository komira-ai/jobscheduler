//! `release macos-pkg --out <dir> --output <file> (--version <V> | --below <V> --tags <file>)
//! [--base-url <url>]`: one signed js-agent package of macOS, as the package repository serves
//! it, for the black-box tests of `.github/workflows/js-agent-macos-blackbox.yml`.
//!
//! - `--version V`: the package of `V`.
//! - `--below V --tags <file>`: the package of the newest release below `V` that has one: the
//!   versions of the file's `vMAJOR.MINOR.PATCH` lines (the repository's tags, `git tag --list
//!   'v*' --merged HEAD`; any other line is passed over) below `V`, newest first, each asked for
//!   in turn, at most [`CANDIDATES`] of them. A release whose run built or signed no package has
//!   none in the repository (it answers no `SHA256SUMS` for it), and no host runs it.
//!
//! The package is read from `<base>/pkg/jobscheduler/<version>/` ([`super::super::channels::pkg`]
//! puts it there): its `SHA256SUMS` first, then the package, whose SHA-256 must be the one the
//! sums list for its file name, or nothing is written. It is written to
//! `<out>/js-agent-<version>-macos-arm64.pkg` (the directory made when missing), and the line
//! `version=<version>` is appended to `--output` (on Actions, `GITHUB_OUTPUT`) and printed.
//! The download is a flag of [`run`], so a test hands a stand-in.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::Digest as _;

use super::{Error, Version, io_error};
use crate::channels::pkg::{PREFIX, file_name};

/// Where the package repository is served.
pub const BASE_URL: &str = "https://packages.komira.ai";

/// The most releases below `--below` that are asked for a package.
pub const CANDIDATES: usize = 10;

/// The arguments of `release macos-pkg`.
#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("which").required(true).args(["version", "below"])))]
pub struct MacosPkgArgs {
    /// The version whose package is fetched, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: Option<String>,
    /// Fetch the package of the newest release below this version that has one.
    #[arg(long, requires = "tags")]
    pub below: Option<String>,
    /// The repository's tags, one a line, for `--below`.
    #[arg(long)]
    pub tags: Option<PathBuf>,
    /// The directory the package is written to.
    #[arg(long)]
    pub out: PathBuf,
    /// The file the `version=<version>` line is appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: PathBuf,
    /// The package repository.
    #[arg(long, default_value = BASE_URL)]
    pub base_url: String,
}

/// The `release macos-pkg` verb, downloading with `get` (the body of a URL, or why not): the line
/// to print.
pub fn run(
    a: &MacosPkgArgs,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<String, Error> {
    let base = a.base_url.trim_end_matches('/');
    let (version, bytes) = match (&a.version, &a.below, &a.tags) {
        (Some(version), _, _) => {
            let version: Version = version.parse()?;
            (version, fetch(base, version, get)?)
        }
        (None, Some(below), Some(tags)) => earlier(base, below.parse()?, tags, get)?,
        _ => {
            return Err(Error::Usage(
                "give --version, or --below with --tags".to_owned(),
            ));
        }
    };
    std::fs::create_dir_all(&a.out).map_err(|e| io_error(&a.out, &e))?;
    let path = a.out.join(file_name(&version.to_string()));
    std::fs::write(&path, &bytes).map_err(|e| io_error(&path, &e))?;
    let line = format!("version={version}");
    append(&a.output, &line)?;
    Ok(line)
}

/// The newest release below `below` among the tags of the file `tags` whose package the
/// repository serves, with the package's bytes.
fn earlier(
    base: &str,
    below: Version,
    tags: &Path,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(Version, Vec<u8>), Error> {
    let text = std::fs::read_to_string(tags).map_err(|e| io_error(tags, &e))?;
    let mut versions: Vec<Version> = text
        .lines()
        .filter_map(|line| Version::parse_tag(line.trim()))
        .filter(|version| *version < below)
        .collect();
    versions.sort_unstable_by_key(|version| std::cmp::Reverse(*version));
    versions.dedup();
    let mut tried = Vec::new();
    for version in versions.into_iter().take(CANDIDATES) {
        match sums(base, version, get) {
            Ok(listed) => return Ok((version, package(base, version, &listed, get)?)),
            Err(why) => tried.push(format!("{version} ({why})")),
        }
    }
    Err(Error::NoEarlierPackage {
        below: below.to_string(),
        tried: match tried.is_empty() {
            true => "no tag below it".to_owned(),
            false => tried.join(", "),
        },
    })
}

/// The package of `version`, checked against its `SHA256SUMS`.
fn fetch(
    base: &str,
    version: Version,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, Error> {
    let url = sums_url(base, version);
    let listed = sums(base, version, get).map_err(|why| Error::Download { url, why })?;
    package(base, version, &listed, get)
}

/// The text of the `SHA256SUMS` of `version`, or why it could not be read.
fn sums(
    base: &str,
    version: Version,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<String, String> {
    let bytes = get(&sums_url(base, version))?;
    String::from_utf8(bytes).map_err(|_| "not UTF-8 text".to_owned())
}

/// The package of `version`, whose SHA-256 must be the one `listed` (its `SHA256SUMS`) gives its
/// file name.
fn package(
    base: &str,
    version: Version,
    listed: &str,
    get: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, Error> {
    let name = file_name(&version.to_string());
    let want = listed
        .lines()
        .find_map(|line| {
            let (sum, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start().trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
        })
        .ok_or_else(|| Error::NotListed {
            url: sums_url(base, version),
            file: name.clone(),
        })?;
    let url = format!("{}{name}", dir_url(base, version));
    let bytes = get(&url).map_err(|why| Error::Download {
        url: url.clone(),
        why,
    })?;
    let found = hex::encode(sha2::Sha256::digest(&bytes));
    if found != want {
        return Err(Error::PackageSum {
            url,
            found,
            listed: want,
        });
    }
    Ok(bytes)
}

/// The directory of `version` in the repository, ending in `/`.
fn dir_url(base: &str, version: Version) -> String {
    format!("{base}/{PREFIX}/{version}/")
}

/// The `SHA256SUMS` of `version`.
fn sums_url(base: &str, version: Version) -> String {
    format!("{}SHA256SUMS", dir_url(base, version))
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

#[cfg(test)]
#[path = "macos_pkg_tests.rs"]
mod tests;
