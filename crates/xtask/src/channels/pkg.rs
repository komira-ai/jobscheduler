//! The macOS installer channel: `js-agent-<version>-macos-arm64.pkg`, the signed installer
//! package of the agent, on `packages.komira.ai`, in the bucket of the apt repository, below
//! `pkg/jobscheduler/<version>/`. A Mac's hostkeeper installs it as a `macos-pkg` entry that
//! `release desired --pkg-url` pins by the package's SHA-256.
//!
//! - `channels pkg upload --dir <dir> --version <V> --bucket <b> --work <dir> [--aws <program>]`
//!   ([`upload`]): the package, then the `SHA256SUMS` beside it, each once. Job `pkg-upload` of
//!   `release.yml`: the cloud role, no key. The package is signed by the workflow that built it;
//!   nothing here signs it, it moves the bytes `SHA256SUMS` lists.
//!
//! The program that talks to the outside (`aws`) is a flag, so a test hands a stand-in
//! ([`super::aws::Aws`]).

pub mod upload;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::aws::RealAws;
use super::{Error, is_bucket, is_plain_version};

/// The key prefix of the repository's `pkg` directory that the publishing role may write: the
/// directory of a version is `<PREFIX>/<version>/`.
pub const PREFIX: &str = "pkg/jobscheduler";

/// The cache header of a file of a version: it never changes under its URL.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The `channels pkg` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// The package and its `SHA256SUMS` into `pkg/jobscheduler/<version>/`, each once.
    Upload {
        /// The directory holding the signed package and the `SHA256SUMS` that lists it.
        #[arg(long)]
        dir: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The repository's bucket.
        #[arg(long)]
        bucket: String,
        /// A directory for the files the verb makes.
        #[arg(long)]
        work: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
}

/// What `upload` is asked.
pub struct Request<'a> {
    /// The directory holding the package and its `SHA256SUMS`.
    pub dir: &'a Path,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
    /// The repository's bucket.
    pub bucket: &'a str,
    /// A directory for the files the verb makes.
    pub work: &'a Path,
}

/// The file name of the package of `version`.
#[must_use]
pub fn file_name(version: &str) -> String {
    format!("js-agent-{version}-macos-arm64.pkg")
}

/// The key prefix of the directory of `version`, ending in `/`.
#[must_use]
pub fn version_prefix(version: &str) -> String {
    format!("{PREFIX}/{version}/")
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Upload {
            dir,
            version,
            bucket,
            work,
            aws,
        } => upload::upload(
            &RealAws(aws),
            &Request {
                dir: &dir,
                version: &version,
                bucket: &bucket,
                work: &work,
            },
        ),
    }
}

/// The flags held to their forms, before anything is read.
pub(crate) fn check_request(r: &Request) -> Result<(), Error> {
    if !is_plain_version(r.version) {
        return Err(Error::Usage(format!(
            "--version {:?} is not <major>.<minor>.<patch>",
            r.version
        )));
    }
    if !is_bucket(r.bucket) {
        return Err(Error::Usage(format!(
            "--bucket {:?} is not a bucket name",
            r.bucket
        )));
    }
    Ok(())
}
