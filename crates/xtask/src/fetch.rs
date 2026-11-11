//! `fetch --url <https url> --sha256 <hex> --out <path> [--extract tar.gz --member <path>]
//! [--cache <path>]`: installs a pinned tool for a CI job.
//!
//! The order is the point:
//!
//! 1. The bytes are read into memory. With `--cache` and a file at that path (the actions
//!    cache restored it), they are that file's, and nothing is downloaded. Otherwise they
//!    are the whole download, over HTTPS (redirects followed, each one HTTPS too; the
//!    host's trusted roots; a status other than 2xx is an error). With `--cache`, a failed
//!    download is reported in one line saying the file was not in the cache either.
//! 2. Their SHA-256 is compared with `--sha256`, whichever way they came. A mismatch is
//!    refused here, before any byte is unpacked or written: nothing is created on disk.
//!    A cached file that does not match is refused too, not replaced by a download.
//! 3. They are unpacked in memory: as is, or one regular file (`--member`, its path
//!    inside the archive) out of a gzip tar (`--extract tar.gz`).
//! 4. With `--cache` and no file there, the download as it came (the archive, not the
//!    file taken out of it) is written to the `--cache` path, for the actions cache to
//!    save: only a download that matched the pin and unpacked is kept.
//! 5. The file is written to `<out>.part` beside `--out`, made executable (0755) and
//!    renamed over `--out`. When writing or renaming fails, `<out>.part` is removed, so
//!    `--out` holds either the whole new file or what it held before.
//!
//! Steps 4 and 5 write through a `.part` file the same way; the kept download is mode 0644.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use sha2::Digest;

/// The largest download read: well above the biggest pinned tool (about 60 MB), and a
/// bound on the memory a wrong URL can take.
pub const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// How long one download may take, redirects included.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// A SHA-256 digest, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        Sha256(sha2::Sha256::digest(bytes).into())
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A `--sha256` value that is not 64 lowercase hex digits.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a SHA-256: {0:?} (expected 64 lowercase hex digits)")]
pub struct NotASha256(String);

impl FromStr for Sha256 {
    type Err = NotASha256;

    fn from_str(s: &str) -> Result<Sha256, NotASha256> {
        let mut digest = [0; 32];
        let lowercase = s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        match hex::decode_to_slice(s, &mut digest) {
            Ok(()) if lowercase => Ok(Sha256(digest)),
            _ => Err(NotASha256(s.to_owned())),
        }
    }
}

/// What `--extract` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// A gzip-compressed tar: the file is its `--member`.
    #[value(name = "tar.gz")]
    TarGz,
}

/// How the installed file comes out of the download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unpack {
    /// The download is the file.
    Raw,
    /// The download is a gzip-compressed tar archive; the file is the regular file at
    /// `member` in it.
    TarGz {
        /// The path of the file inside the archive.
        member: PathBuf,
    },
}

impl Unpack {
    /// `--extract` and `--member` as given; `Err` names the flag that is missing or
    /// out of place.
    pub fn new(extract: Option<Format>, member: Option<PathBuf>) -> Result<Unpack, String> {
        match (extract, member) {
            (None, None) => Ok(Unpack::Raw),
            (Some(Format::TarGz), Some(member)) => Ok(Unpack::TarGz { member }),
            (Some(Format::TarGz), None) => {
                Err("--extract tar.gz needs --member, the file to take".to_owned())
            }
            (None, Some(_)) => {
                Err("--member is a file in a tar: it needs --extract tar.gz".to_owned())
            }
        }
    }

    /// The file this unpacking makes of `bytes`.
    fn apply(&self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        match self {
            Unpack::Raw => Ok(bytes.to_vec()),
            Unpack::TarGz { member } => {
                let mut tar = Vec::new();
                flate2::read::MultiGzDecoder::new(bytes)
                    .read_to_end(&mut tar)
                    .map_err(unpack_error)?;
                tar_member(&tar, member)
            }
        }
    }
}

/// Why a fetch failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The download failed: a connection, certificate, redirect or status error.
    #[error("download {url}: {why}")]
    Download {
        /// The URL asked for.
        url: String,
        /// The client's reason.
        why: String,
    },
    /// The download is not the pinned file.
    #[error("the download's SHA-256 is {actual}, not the pinned {expected}; nothing was installed")]
    Mismatch {
        /// The pinned digest.
        expected: Sha256,
        /// The digest of what was downloaded.
        actual: Sha256,
    },
    /// The file was not in the cache, and the download failed.
    #[error(
        "{cache:?} was not in the actions cache, and the download failed: {download}; nothing was installed"
    )]
    NotCached {
        /// The `--cache` path, which held no file.
        cache: PathBuf,
        /// The download's error, on one line.
        download: String,
    },
    /// The cached file is not the pinned file.
    #[error(
        "the cached {cache:?} has SHA-256 {actual}, not the pinned {expected}; nothing was installed"
    )]
    CachedMismatch {
        /// The `--cache` path.
        cache: PathBuf,
        /// The pinned digest.
        expected: Sha256,
        /// The digest of the cached file.
        actual: Sha256,
    },
    /// The `--cache` path could not be read or written.
    #[error("the cache file {cache:?}: {why}")]
    Cache {
        /// The `--cache` path.
        cache: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// The archive could not be decoded.
    #[error("unpack the download: {0}")]
    Unpack(String),
    /// The archive holds no entry at the requested path.
    #[error("the archive has no {0:?}")]
    NoMember(PathBuf),
    /// The entry at the requested path is a directory, a link or another special file.
    #[error("{0:?} in the archive is not a regular file")]
    NotAFile(PathBuf),
    /// The file could not be written to its place.
    #[error("install {path:?}: {why}")]
    Install {
        /// The file that was to be installed.
        path: PathBuf,
        /// The system's reason.
        why: String,
    },
}

/// The error for an archive that could not be decoded.
fn unpack_error(e: impl fmt::Display) -> Error {
    Error::Unpack(e.to_string())
}

/// The roots the host trusts (the system store), for [`fetch`] and [`download`].
pub fn host_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The TLS settings that trust exactly `roots`, over rustls with ring.
pub fn tls_config(roots: &[CertificateDer<'static>]) -> ureq::tls::TlsConfig {
    let roots = roots
        .iter()
        .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
        .collect();
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build()
}

/// The body of `url`, over HTTPS only, trusting `roots`.
pub fn download(url: &str, roots: &[CertificateDer<'static>]) -> Result<Vec<u8>, Error> {
    download_within(url, roots, DOWNLOAD_TIMEOUT)
}

/// [`download`], given up after `timeout` (redirects included).
pub fn download_within(
    url: &str,
    roots: &[CertificateDer<'static>],
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .tls_config(tls_config(roots))
        .build()
        .into();
    let failed = |e: ureq::Error| Error::Download {
        url: url.to_owned(),
        why: e.to_string(),
    };
    let mut response = agent.get(url).call().map_err(failed)?;
    let body = response.body_mut().with_config().limit(MAX_DOWNLOAD);
    body.read_to_vec().map_err(failed)
}

/// Checks `bytes` against `sha256`, unpacks them and installs the file at `out`.
pub fn install(bytes: &[u8], sha256: &Sha256, unpack: &Unpack, out: &Path) -> Result<(), Error> {
    let file = checked(bytes, sha256, unpack)?;
    place(out, &file)
}

/// The file `unpack` makes of `bytes`, once their digest is `sha256`.
fn checked(bytes: &[u8], sha256: &Sha256, unpack: &Unpack) -> Result<Vec<u8>, Error> {
    let actual = Sha256::of(bytes);
    if actual != *sha256 {
        return Err(Error::Mismatch {
            expected: *sha256,
            actual,
        });
    }
    unpack.apply(bytes)
}

/// Writes `file` to `out`, mode 0755.
fn place(out: &Path, file: &[u8]) -> Result<(), Error> {
    write_file(out, file, 0o755).map_err(|e| Error::Install {
        path: out.to_owned(),
        why: e.to_string(),
    })
}

/// [`download`]s `url` and [`install`]s it at `out`: the line to print. With `cache`, the
/// file there is checked and installed in place of the download when it exists; when it
/// does not, the download is checked, kept there, and installed.
pub fn fetch(
    url: &str,
    sha256: &Sha256,
    unpack: &Unpack,
    out: &Path,
    cache: Option<&Path>,
    roots: &[CertificateDer<'static>],
) -> Result<String, Error> {
    let Some(cache) = cache else {
        let bytes = download(url, roots)?;
        install(&bytes, sha256, unpack, out)?;
        return Ok(format!(
            "ok: {url} has SHA-256 {sha256}; installed {}",
            out.display()
        ));
    };
    let cache_error = |e: std::io::Error| Error::Cache {
        cache: cache.to_owned(),
        why: e.to_string(),
    };
    match std::fs::read(cache) {
        Ok(bytes) => {
            let file = checked(&bytes, sha256, unpack).map_err(|e| match e {
                Error::Mismatch { expected, actual } => Error::CachedMismatch {
                    cache: cache.to_owned(),
                    expected,
                    actual,
                },
                other => other,
            })?;
            place(out, &file)?;
            Ok(format!(
                "ok: the cached {} of {url} has SHA-256 {sha256}; installed {}",
                cache.display(),
                out.display()
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let bytes = download(url, roots).map_err(|e| Error::NotCached {
                cache: cache.to_owned(),
                download: e.to_string().replace(['\r', '\n'], " "),
            })?;
            let file = checked(&bytes, sha256, unpack)?;
            write_file(cache, &bytes, 0o644).map_err(cache_error)?;
            place(out, &file)?;
            Ok(format!(
                "ok: {url} has SHA-256 {sha256}; kept it at {} and installed {}",
                cache.display(),
                out.display()
            ))
        }
        Err(e) => Err(cache_error(e)),
    }
}

/// The file `out` is written as before it is renamed over `out`.
pub fn part_path(out: &Path) -> PathBuf {
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Writes `data` to `out` (mode `mode`) through [`part_path`], removing the part file
/// when a step fails.
pub(crate) fn write_file(out: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    out.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    let part = part_path(out);
    let written = std::fs::write(&part, data)
        .and_then(|()| std::fs::set_permissions(&part, std::fs::Permissions::from_mode(mode)))
        .and_then(|()| std::fs::rename(&part, out));
    if written.is_err() {
        // A part file that cannot be removed is reported by the write error itself.
        let _ = std::fs::remove_file(&part);
    }
    written
}

/// The contents of the regular file at `member` in the tar archive `tar`.
fn tar_member(tar: &[u8], member: &Path) -> Result<Vec<u8>, Error> {
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(unpack_error)? {
        let mut entry = entry.map_err(unpack_error)?;
        if entry.path().map_err(unpack_error)? != member {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(Error::NotAFile(member.to_owned()));
        }
        let mut file = Vec::new();
        entry.read_to_end(&mut file).map_err(unpack_error)?;
        return Ok(file);
    }
    Err(Error::NoMember(member.to_owned()))
}

#[cfg(test)]
mod tests;
