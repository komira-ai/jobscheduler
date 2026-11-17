//! The signed Mac installer package of a release, as `release desired --pkg-url <base>` reads
//! it: `<base>/<X.Y.Z>/SHA256SUMS` and the package `<base>/<X.Y.Z>/js-agent-<X.Y.Z>-macos-arm64.pkg`,
//! the files `channels pkg upload` put there, read from the public host over HTTPS.
//!
//! A directory with no `SHA256SUMS` means the release has no Mac package yet: the release is then
//! set for the Linux platforms alone. The host answers a file that is not there with a `404`, or
//! with a `403` when it is a CDN over an object store it may not list (`packages.komira.ai` is
//! CloudFront over S3, which answers every missing object so), and both mean no sums. Any other failure to read it is an error,
//! and so is a package whose SHA-256 is not the one the sums list, so a half-published or
//! replaced package is never pinned.

use std::path::Path;

use super::repo::sums_digest;
use super::{Error, SUMS, capture, strings};
use crate::fetch::Sha256;
use crate::release::{Version, io_error};

/// What curl says under `--fail` of the answers that mean the sums are not there: `404`, and the
/// `403` a store that may not be listed answers for a missing object.
const ABSENT: [&str; 2] = ["returned error: 404", "returned error: 403"];

/// The Mac package of a release.
#[derive(Debug, PartialEq, Eq)]
pub struct Package {
    /// The SHA-256 of the signed package, as the sums list it and the file has it.
    pub digest: String,
    /// The size of the package in bytes.
    pub bytes: u64,
}

/// The file name of the package of `version`.
#[must_use]
pub fn file_name(version: Version) -> String {
    format!("js-agent-{version}-macos-arm64.pkg")
}

/// The arguments of the `curl` that reads `url` into `file`: HTTPS only, TLS 1.2 or later, a
/// failure on an HTTP error status and five retries of what is transient (a `404` or a `403` is
/// not: it is an answer), with a time for a package of some tens of megabytes.
fn curl_args(file: &Path, url: &str) -> Vec<String> {
    let mut args = strings(&[
        "--proto",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--retry",
        "5",
        "--retry-delay",
        "20",
        "--max-time",
        "600",
        "--output",
    ]);
    args.push(file.display().to_string());
    args.push(url.to_owned());
    args
}

/// The package of `version` at `base`, or `None` when its directory has no `SHA256SUMS` (a `404`
/// or a `403` for it).
///
/// # Errors
/// [`Error::Refused`] for sums that list no package or a package whose digest is not the one
/// they list; [`Error::Release`] for a `curl` that fails other than with a `404` or a `403` for
/// the sums (any failure for the package they list), or a file that cannot be made or read.
pub fn fetch(
    curl: &str,
    dir: &Path,
    base: &str,
    version: Version,
) -> Result<Option<Package>, Error> {
    let dir = dir.join("pkg");
    std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, &e))?;
    let name = file_name(version);
    let sums_file = dir.join(SUMS);
    let sums_url = format!("{base}/{version}/{SUMS}");
    match capture(curl, &curl_args(&sums_file, &sums_url)) {
        Ok(_) => {}
        Err(crate::release::Error::Failed { stderr, .. })
            if ABSENT.iter().any(|absent| stderr.contains(absent)) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    let sums = std::fs::read_to_string(&sums_file).map_err(|e| io_error(&sums_file, &e))?;
    let Some(listed) = sums_digest(&sums, &name)? else {
        return Err(Error::Refused(vec![format!("{sums_url} lists no {name}")]));
    };
    let file = dir.join(&name);
    capture(curl, &curl_args(&file, &format!("{base}/{version}/{name}")))?;
    let bytes = std::fs::read(&file).map_err(|e| io_error(&file, &e))?;
    let digest = Sha256::of(&bytes).to_string();
    if digest != listed {
        return Err(Error::Refused(vec![format!(
            "{name} at {base}/{version} has sha256 {digest}, but its {SUMS} lists {listed}"
        )]));
    }
    Ok(Some(Package {
        digest,
        bytes: bytes.len() as u64,
    }))
}

#[cfg(test)]
mod tests;
