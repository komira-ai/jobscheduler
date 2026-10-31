//! The js-agent installer package for macOS.
//!
//! STUB: the body arrives with the code commit; the tests of `tests/macos_pkg.rs` come first.

use std::path::PathBuf;

use crate::release::Version;

/// The package identifier.
pub const IDENTIFIER: &str = "com.komira.js-agent";

/// Whether an entry is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// One entry of the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path below the volume, without a leading slash.
    pub path: String,
    /// File or directory.
    pub kind: Kind,
    /// The permission bits.
    pub mode: u32,
}

/// The payload of the package of `version`: every file and every directory.
#[must_use]
pub fn entries(_version: &str) -> Vec<Entry> {
    Vec::new()
}

/// The `postinstall` script of the package of `version`.
#[must_use]
pub fn postinstall_script(_version: &str) -> String {
    String::new()
}

/// The arguments of `pkg stage-macos`.
#[derive(clap::Args)]
pub struct StageArgs {
    /// The release version.
    #[arg(long)]
    pub version: Version,
    /// The directory with the built `js-agent`, `js-mac-session` and `js-mac-provision`.
    #[arg(long)]
    pub bin_dir: PathBuf,
    /// The license file.
    #[arg(long)]
    pub license: PathBuf,
    /// The agent's plist.
    #[arg(long)]
    pub agent_plist: PathBuf,
    /// The helper's plist.
    #[arg(long)]
    pub session_plist: PathBuf,
    /// The update hook's declaration.
    #[arg(long)]
    pub declaration: PathBuf,
    /// The directory to make, which must not exist: `root/` and `scripts/` go in it.
    #[arg(long)]
    pub out: PathBuf,
}

/// The arguments of `pkg check-macos`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// The package.
    #[arg(long)]
    pub pkg: PathBuf,
    /// The version it must be.
    #[arg(long)]
    pub version: Version,
}

/// What was read back from a package.
pub struct Inspected {
    /// `lsbom` of the component's bill of materials.
    pub bom: String,
    /// The files of the component's scripts: name and bytes.
    pub scripts: Vec<(String, Vec<u8>)>,
    /// The component's `PackageInfo`.
    pub package_info: String,
}

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// The package is not the package.
    #[error("the package is not the js-agent package: {}", .0.join("; "))]
    Refused(Vec<String>),
    /// `pkgutil` or `lsbom` failed.
    #[error("{0}")]
    Tool(String),
    /// The package is read with Apple's tools.
    #[error("pkg check-macos reads the package with pkgutil and lsbom, which only a Mac has")]
    NeedsMac,
}

/// Lays the payload and the scripts down in `args.out`: the line to print.
///
/// # Errors
/// [`Error`].
pub fn stage(_args: &StageArgs) -> Result<String, Error> {
    Ok(String::new())
}

/// What is wrong with a package, one line each; empty when it is the package of `version`.
#[must_use]
pub fn problems(_inspected: &Inspected, _version: &str) -> Vec<String> {
    Vec::new()
}

/// Reads the package with `pkgutil` and `lsbom` and refuses it unless it is the package.
///
/// # Errors
/// [`Error`].
pub fn check(_args: &CheckArgs) -> Result<String, Error> {
    Ok(String::new())
}
