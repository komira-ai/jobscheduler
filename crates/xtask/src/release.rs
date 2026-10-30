//! `release plan` and `release stamp`: the decisions behind the continuous release
//! (`.github/workflows/release.yml`, docs/release.md), as code with tests.
//!
//! - [`plan`]: what the commit that passed `ci` on main would release: whether anything
//!   that ships changed since the latest `v*` tag, and the version if so.
//! - [`stamp`]: sets the release version in the CI checkout (`Cargo.toml` and `Cargo.lock`)
//!   before the build, so `--version` of every binary prints it. Nothing is committed.
//!
//! What ships is computed, not listed: the workspace crates the shipped binaries
//! (`js-lease-server`, `js` from `js-cli`, `js-agent`, `js-bundle` from `js-bundle-cli`, `js-mac-session`, `js-mac-provision`) depend on, through normal and build
//! dependencies (`cargo metadata`). Every file of such a crate ships (`src/**`, manifest,
//! `build.rs`, an `include_str!`'d sample configuration) except its `tests/`, `benches/`,
//! `examples/`, markdown and `Cargo.lock`; so do the root `Cargo.toml`, `Cargo.lock`,
//! `rust-toolchain.toml` and everything under `.cargo/`. Everything else does not: docs,
//! markdown, `.github/`, and the crates no shipped binary depends on (`xtask`,
//! `js-coverage`, `js-conformance`). A file in one of the excluded places that a shipped
//! source includes (`include_str!` and its kin, read by [`include`]) ships too.
//!
//! The version: one patch above the latest tag, unless the workspace version in
//! `Cargo.toml` is above it, which is used as is. No tag yet: the workspace version.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::Deserialize;

pub mod include;
pub mod plan;
pub mod stamp;

#[cfg(test)]
mod tests;

/// Why a release verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A string that is not `MAJOR.MINOR.PATCH`.
    #[error("not a version MAJOR.MINOR.PATCH: {0:?}")]
    NotAVersion(String),
    /// A program could not be started.
    #[error("cannot run {program}: {why}")]
    Spawn {
        /// The program.
        program: String,
        /// The operating system's reason.
        why: String,
    },
    /// A program ran and failed.
    #[error("{command} failed ({status}): {stderr}")]
    Failed {
        /// The command line.
        command: String,
        /// How it ended.
        status: String,
        /// What it printed on stderr.
        stderr: String,
    },
    /// A program printed bytes that are not UTF-8.
    #[error("{command} printed bytes that are not UTF-8")]
    NotUtf8 {
        /// The command line.
        command: String,
    },
    /// A file could not be read or written.
    #[error("{path}: {why}")]
    Io {
        /// The file.
        path: String,
        /// The operating system's reason.
        why: String,
    },
    /// The output of `cargo metadata` is not what this reads.
    #[error("cargo metadata: {0}")]
    Metadata(String),
    /// The root `Cargo.toml` has no readable `[workspace.package] version`.
    #[error("Cargo.toml: {0}")]
    Manifest(String),
    /// The root `Cargo.toml` has no `version` line under `[workspace.package]` to set.
    #[error("Cargo.toml has no version line under [workspace.package]")]
    NoVersionLine,
    /// A shipped crate, or a path dependency of one, is not a workspace member.
    #[error("crate {0} is not a workspace member")]
    UnknownCrate(String),
    /// A workspace member's manifest is not below the workspace root.
    #[error("the manifest of {0} is not below the workspace root")]
    OutsideWorkspace(String),
    /// A shipped source includes a file by something other than a plain string literal, so
    /// the file it compiles in cannot be told.
    #[error("{file}:{line}: an include whose path is not a plain string literal")]
    Include {
        /// The source file.
        file: String,
        /// The line of the include.
        line: usize,
    },
    /// The checkout is not at the commit the run is about.
    #[error("the checkout is at {actual}, not at --head {expected}")]
    WrongCheckout {
        /// The commit asked for.
        expected: String,
        /// The commit checked out.
        actual: String,
    },
}

/// A release version, `MAJOR.MINOR.PATCH` with plain decimal numbers (no `v`, no suffix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// The major number.
    pub major: u64,
    /// The minor number.
    pub minor: u64,
    /// The patch number.
    pub patch: u64,
}

impl Version {
    /// The tag of this version, `vMAJOR.MINOR.PATCH`.
    pub fn tag(self) -> String {
        format!("v{self}")
    }

    /// The version a tag names: `None` for anything but `v` and a version.
    pub fn parse_tag(tag: &str) -> Option<Version> {
        tag.strip_prefix('v')?.parse().ok()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = Error;

    fn from_str(s: &str) -> Result<Version, Error> {
        // A number is its own canonical text: "01" and "+1" are not versions.
        let numbers: Option<Vec<u64>> = s
            .split('.')
            .map(|part| part.parse().ok().filter(|n: &u64| n.to_string() == part))
            .collect();
        match numbers.as_deref() {
            Some(&[major, minor, patch]) => Ok(Version {
                major,
                minor,
                patch,
            }),
            _ => Err(Error::NotAVersion(s.to_owned())),
        }
    }
}

/// The highest version among `tags`, by version order (v0.1.10 is above v0.1.9); a tag that
/// is not `v` and a version is not a release and is skipped.
pub fn latest<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<Version> {
    tags.into_iter().filter_map(Version::parse_tag).max()
}

/// The version to release: one patch above `latest`, unless the workspace version `cargo`
/// is above it (a minor or major bump made by a reviewed change), which is used as is. With
/// no tag yet, `cargo`.
pub fn next(latest: Option<Version>, cargo: Version) -> Version {
    match latest {
        Some(tagged) if tagged >= cargo => Version {
            patch: tagged.patch + 1,
            ..tagged
        },
        _ => cargo,
    }
}

/// The part of `cargo metadata --no-deps` this reads.
#[derive(Deserialize)]
pub struct Metadata {
    workspace_root: String,
    packages: Vec<Package>,
}

/// One workspace member.
#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: String,
    dependencies: Vec<Dependency>,
}

/// One declared dependency.
#[derive(Deserialize)]
struct Dependency {
    name: String,
    /// `None` for a normal dependency, `"dev"` or `"build"`.
    kind: Option<String>,
    /// Set for a dependency on a crate by path, which is a workspace member here.
    path: Option<String>,
}

impl Metadata {
    /// Reads the JSON `cargo metadata --no-deps --format-version 1` printed.
    pub fn parse(json: &str) -> Result<Metadata, Error> {
        serde_json::from_str(json).map_err(|e| Error::Metadata(e.to_string()))
    }

    /// The directories (relative to the workspace root) of the crates `roots` name and of
    /// every workspace crate they depend on through normal or build dependencies. A
    /// dev-dependency is not shipped: only tests run it.
    pub fn shipped_dirs(&self, roots: &[String]) -> Result<BTreeSet<String>, Error> {
        let mut dirs = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<&str> = roots.iter().map(String::as_str).collect();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name) {
                continue;
            }
            let package = self
                .packages
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(|| Error::UnknownCrate(name.to_owned()))?;
            dirs.insert(self.dir_of(package)?);
            queue.extend(
                package
                    .dependencies
                    .iter()
                    .filter(|d| d.path.is_some() && d.kind.as_deref() != Some("dev"))
                    .map(|d| d.name.as_str()),
            );
        }
        Ok(dirs)
    }

    /// The directory of a member's manifest, relative to the workspace root.
    fn dir_of(&self, package: &Package) -> Result<String, Error> {
        Path::new(&package.manifest_path)
            .strip_prefix(&self.workspace_root)
            .ok()
            .and_then(Path::parent)
            .and_then(Path::to_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::OutsideWorkspace(package.name.clone()))
    }
}

/// The files at the root of the repository that ship in every binary.
const ROOT_FILES: [&str; 3] = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

/// The directory at the root whose files (`config.toml`: rustflags, linker, build settings)
/// change how every binary is built.
const CARGO_DIR: &str = ".cargo/";

/// Whether a change to `path` (relative to the repository root) changes a shipped binary:
/// a root build input (including everything under `.cargo/`), or a file of a shipped crate.
/// Everything in a shipped crate ships (a file can be `include_str!`'d from anywhere in
/// it) except what is never compiled into a binary: `tests/`, `benches/`, `examples/`,
/// markdown outside `src/`, and a crate-level `Cargo.lock` (cargo reads the workspace's);
/// a file of those kinds that a shipped source includes still ships ([`first_shipped`]).
pub fn is_shipped(path: &str, dirs: &BTreeSet<String>) -> bool {
    ROOT_FILES.contains(&path)
        || path.starts_with(CARGO_DIR)
        || dirs.iter().any(|dir| {
            path.strip_prefix(dir.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(crate_file_ships)
        })
}

/// Whether the file `rest`, relative to a shipped crate's directory, ships.
fn crate_file_ships(rest: &str) -> bool {
    if rest.starts_with("src/") {
        return true;
    }
    let never = ["tests/", "benches/", "examples/"];
    !(never.iter().any(|dir| rest.starts_with(dir))
        || rest.ends_with(".md")
        || rest == "Cargo.lock")
}

/// The first of `changed` that ships, if any: a path [`is_shipped`] accepts, or one of
/// `included`, the files the shipped sources compile in ([`include::targets`]), whatever
/// their location.
pub fn first_shipped<'a>(
    changed: &'a [String],
    dirs: &BTreeSet<String>,
    included: &BTreeSet<String>,
) -> Option<&'a str> {
    changed
        .iter()
        .map(String::as_str)
        .find(|path| is_shipped(path, dirs) || included.contains(*path))
}

/// The root manifest as far as the workspace version.
#[derive(Deserialize)]
struct Manifest {
    workspace: ManifestWorkspace,
}

/// The `[workspace]` table.
#[derive(Deserialize)]
struct ManifestWorkspace {
    package: ManifestPackage,
}

/// The `[workspace.package]` table.
#[derive(Deserialize)]
struct ManifestPackage {
    version: String,
}

/// The `[workspace.package] version` of the root manifest `text`.
pub fn workspace_version(text: &str) -> Result<Version, Error> {
    let manifest: Manifest = toml::from_str(text).map_err(|e| Error::Manifest(e.to_string()))?;
    manifest.workspace.package.version.parse()
}

/// Reads a file as text.
pub(crate) fn read(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| io_error(path, &e))
}

/// Writes a file.
pub(crate) fn write(path: &Path, text: &str) -> Result<(), Error> {
    std::fs::write(path, text).map_err(|e| io_error(path, &e))
}

/// An [`Error::Io`] for `path`.
pub(crate) fn io_error(path: &Path, e: &std::io::Error) -> Error {
    Error::Io {
        path: path.display().to_string(),
        why: e.to_string(),
    }
}
