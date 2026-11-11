//! `release image-digest --dist <dir> --repo <owner/repo> --version <X.Y.Z> [--output <file>]`:
//! what the control plane's deploy of `js-server` is given for a release, read from the files the
//! release published (the `cp-deploy-digest` job of `.github/workflows/release.yml`, docs/release.md
//! "The control-plane deploy").
//!
//! The deploy (docker-swarm-updater's reusable deploy workflow) names the image by its manifest
//! digest and by the release's download addresses of the image tar and of its provenance bundle; the
//! updater downloads both, verifies the bundle against this repository's `artifacts.yml` on `main`,
//! and refuses an image whose digest is not the one named. The image is the linux-x86_64 one,
//! [`file_name`]`(version, `[`TARGET`]`)`, the platform the control plane runs.
//!
//! In `--dist`, the directory that holds the release's files as `publish` uploaded them:
//!
//! 1. `SHA256SUMS` lists the image exactly once, and the image is that file, byte for byte;
//! 2. the bundle `<image>.sigstore.json` is there and not empty;
//! 3. the image's `index.json` names exactly one manifest, by a digest `sha256:<64 hex>`, and the
//!    blob `blobs/sha256/<hex>` of the tar hashes to it.
//!
//! Only then it prints `digest=sha256:<hex>`, `image-url=` and `bundle-url=` (the
//! `https://github.com/<repo>/releases/download/v<version>/<file>` of each) and appends the three
//! lines to `--output` (on Actions, `GITHUB_OUTPUT`). Any refusal writes nothing. The repository is
//! written into the addresses, so each of its two halves must be a plain name (letters, digits,
//! `.`, `_` and `-`, starting with a letter or digit).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::server_image::{Target, file_name};
use super::stage::is_name;
use super::{Version, io_error};

#[cfg(test)]
mod tests;

/// The image deployed: the control plane runs on linux-x86_64.
pub const TARGET: Target = Target::X86_64;

/// The release asset that lists the SHA-256 of every other.
pub const SUMS: &str = "SHA256SUMS";

/// Why `release image-digest` gave no digest.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `--repo` is not `<owner>/<name>` of plain names.
    #[error(
        "--repo {0:?} is not <owner>/<name> of ASCII letters, digits, `.`, `_` and `-`, each starting with a letter or digit"
    )]
    BadRepo(String),
    /// A line of `SHA256SUMS` is not `<sha256>  <name>`.
    #[error("{SUMS} line {line} is not `<sha256>  <name>`: {text:?}")]
    Sums {
        /// The line number, from 1.
        line: usize,
        /// The line.
        text: String,
    },
    /// `SHA256SUMS` does not list the image exactly once.
    #[error("{SUMS} lists {name} {times} times, not once")]
    NotListedOnce {
        /// The image's file name.
        name: String,
        /// How many lines name it.
        times: usize,
    },
    /// The image is not the file `SHA256SUMS` lists.
    #[error("{name} has sha256 {actual}, not the {listed} {SUMS} lists")]
    NotTheListedFile {
        /// The image's file name.
        name: String,
        /// The digest `SHA256SUMS` gives.
        listed: String,
        /// The digest of the file.
        actual: String,
    },
    /// The bundle is empty.
    #[error("{0} is empty")]
    EmptyBundle(String),
    /// The image is not an OCI layout with one manifest whose blob matches its digest.
    #[error("{name}: {why}")]
    NotAnImage {
        /// The image's file name.
        name: String,
        /// What is wrong with it.
        why: String,
    },
    /// A file could not be read or written.
    #[error(transparent)]
    Release(#[from] super::Error),
}

/// The arguments of `release image-digest`.
#[derive(clap::Args)]
pub struct DigestArgs {
    /// The directory holding the release's files: `SHA256SUMS`, the image and its bundle.
    #[arg(long)]
    pub dist: PathBuf,
    /// `<owner>/<name>` of the repository the release is published in (on Actions,
    /// `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The release version, `MAJOR.MINOR.PATCH`; the tag is `v` and the version.
    #[arg(long)]
    pub version: Version,
    /// Append the three lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// The lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Is `text` 64 lowercase hex digits?
fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The digest `sha256sum` wrote for `name` in `sums`: a line is 64 lowercase hex digits, a space,
/// then a space or `*` (binary mode) and the name.
///
/// # Errors
/// [`Error::Sums`] for a line that is not of that form; [`Error::NotListedOnce`] when no line or
/// more than one names `name`.
pub fn listed_digest(sums: &str, name: &str) -> Result<String, Error> {
    let mut found = Vec::new();
    for (at, line) in sums.lines().enumerate() {
        let parsed = line
            .split_once(' ')
            .filter(|(digest, _)| is_hex64(digest))
            .and_then(|(digest, rest)| Some((digest, rest.strip_prefix([' ', '*'])?)));
        let Some((digest, listed)) = parsed else {
            return Err(Error::Sums {
                line: at + 1,
                text: line.to_owned(),
            });
        };
        if listed == name {
            found.push(digest.to_owned());
        }
    }
    match found.as_slice() {
        [digest] => Ok(digest.clone()),
        _ => Err(Error::NotListedOnce {
            name: name.to_owned(),
            times: found.len(),
        }),
    }
}

/// The regular files of the tar `bytes`, by path.
fn members(bytes: &[u8]) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in tar::Archive::new(bytes).entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content)?;
            files.insert(path, content);
        }
    }
    Ok(files)
}

/// The manifest digest of the OCI layout tar `bytes`, `sha256:<hex>`: the one manifest its
/// `index.json` names, whose blob hashes to that digest.
///
/// # Errors
/// The reason, when the bytes are not a tar, have no readable `index.json`, name no manifest or
/// more than one, name one by anything but `sha256:<64 hex>`, or lack its blob or hold a blob
/// that does not hash to it.
pub fn manifest_digest(bytes: &[u8]) -> Result<String, String> {
    let files = members(bytes).map_err(|e| format!("not a tar: {e}"))?;
    let index = files
        .get("index.json")
        .ok_or("the layout has no index.json")?;
    let index: serde_json::Value =
        serde_json::from_slice(index).map_err(|e| format!("index.json is not JSON: {e}"))?;
    let manifests = index["manifests"].as_array().map_or(&[][..], Vec::as_slice);
    let [manifest] = manifests else {
        return Err(format!(
            "index.json names {} manifests, not one",
            manifests.len()
        ));
    };
    let digest = manifest["digest"].as_str().unwrap_or_default();
    let hex = digest
        .strip_prefix("sha256:")
        .filter(|hex| is_hex64(hex))
        .ok_or_else(|| format!("index.json names the manifest {digest:?}, not sha256:<64 hex>"))?;
    let blob = format!("blobs/sha256/{hex}");
    let content = files
        .get(&blob)
        .ok_or_else(|| format!("the layout has no {blob}"))?;
    let actual = sha256_hex(content);
    if actual != hex {
        return Err(format!("{blob} has sha256 {actual}"));
    }
    Ok(digest.to_owned())
}

/// Reads `path`.
fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| io_error(path, &e).into())
}

/// Runs `release image-digest`: the three lines, one per line.
///
/// # Errors
/// [`Error::BadRepo`] before anything is read; [`Error::Release`] for a file that cannot be read
/// or the output that cannot be written; the other variants for a release whose files are not as
/// the module docs say.
pub fn run(args: &DigestArgs) -> Result<String, Error> {
    let repo_ok = args
        .repo
        .split_once('/')
        .is_some_and(|(owner, name)| is_name(owner) && is_name(name));
    if !repo_ok {
        return Err(Error::BadRepo(args.repo.clone()));
    }
    let image = file_name(args.version, TARGET);
    let bundle = format!("{image}.sigstore.json");
    let sums = read(&args.dist.join(SUMS))?;
    let listed = listed_digest(&String::from_utf8_lossy(&sums), &image)?;
    let bytes = read(&args.dist.join(&image))?;
    let actual = sha256_hex(&bytes);
    if actual != listed {
        return Err(Error::NotTheListedFile {
            name: image,
            listed,
            actual,
        });
    }
    if read(&args.dist.join(&bundle))?.is_empty() {
        return Err(Error::EmptyBundle(bundle));
    }
    let digest = manifest_digest(&bytes).map_err(|why| Error::NotAnImage {
        name: image.clone(),
        why,
    })?;
    let base = format!(
        "https://github.com/{}/releases/download/{}",
        args.repo,
        args.version.tag()
    );
    let lines = format!("digest={digest}\nimage-url={base}/{image}\nbundle-url={base}/{bundle}");
    if let Some(output) = &args.output {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .and_then(|mut file| writeln!(file, "{lines}"))
            .map_err(|e| io_error(output, &e))?;
    }
    Ok(lines)
}
