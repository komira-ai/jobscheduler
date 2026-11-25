//! `fetch --url <https url> --sha256 <hex> --out <path> [--extract tar.gz|tar.xz --member <path>
//! [--tree] | --extract zst] [--cache <path> [--url <https url>]...]`: installs a pinned tool
//! for a CI job.
//!
//! The order is the point:
//!
//! 1. The bytes are read into memory. With `--cache` and a file at that path (the actions
//!    cache restored it), they are that file's, and nothing is downloaded. Otherwise they
//!    are the whole download, over HTTPS (redirects followed, each one HTTPS too; the
//!    host's trusted roots; a status other than 2xx is an error). Without `--cache` there is
//!    one `--url` and one attempt, given up after [`DOWNLOAD_TIMEOUT`]. With `--cache` and no
//!    file there ([`fetch_cached`]), each `--url` is tried in turn, in `WAITS.len()` rounds
//!    with the pause of [`WAITS`] before each round, each attempt given up after
//!    [`ATTEMPT_TIMEOUT`] ([`first_pinned`]): the first download whose SHA-256 is the pin is
//!    taken, and a download of other bytes is one more failed attempt. When every attempt
//!    failed, one line says the file was not in the cache and names each attempt, in order.
//! 2. Their SHA-256 is compared with `--sha256`, whichever way they came. A mismatch is
//!    refused here, before any byte is unpacked or written: nothing is created on disk.
//!    A cached file that does not match is refused too, not replaced by a download.
//! 3. They are unpacked in memory: as is, or one regular file (`--member`, its path
//!    inside the archive) out of a gzip or xz tar (`--extract tar.gz` or `tar.xz`), or one
//!    zstd-compressed file decompressed whole (`--extract zst`, the form buck2's releases
//!    take), or with
//!    `--tree` every directory and regular file under the directory `--member` (a tool that
//!    needs the files beside it, such as a compiler and its headers). A tree holding a link
//!    or another special file, or an entry whose path climbs out of `--member`, is refused.
//! 4. With `--cache` and no file there, the download as it came (the archive, not the
//!    file taken out of it) is written to the `--cache` path, for the actions cache to
//!    save: only a download that matched the pin and unpacked is kept.
//! 5. The file is written to `<out>.part` beside `--out`, made executable (0755) and
//!    renamed over `--out`. When writing or renaming fails, `<out>.part` is removed, so
//!    `--out` holds either the whole new file or what it held before. A tree is written the
//!    same way, as the directory `<out>.part` (a file executable in the archive 0755, any
//!    other 0644), and renamed to `--out`, which must not be a directory that holds anything.
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

/// How long one download without `--cache` may take, redirects included.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// The pause before each round of downloads of [`first_pinned`] (a cached fetch's, and
/// `debs`'s): none before the first.
pub const WAITS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long one attempt of [`fetch_cached`] may take, redirects included: a stalled host gives
/// way to the next `--url` well before [`DOWNLOAD_TIMEOUT`].
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// The download of one URL: its bytes, or why it failed.
pub type Get<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

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
    /// An xz-compressed tar: the file is its `--member`.
    #[value(name = "tar.xz")]
    TarXz,
    /// One zstd-compressed file: the file is the download decompressed (no `--member`).
    #[value(name = "zst")]
    Zst,
}

impl Format {
    /// What `bytes` holds in this compression: a tar archive for the tar formats, the file
    /// itself for zst.
    fn decode(self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        let mut tar = Vec::new();
        match self {
            Format::TarGz => flate2::read::MultiGzDecoder::new(bytes)
                .read_to_end(&mut tar)
                .map(drop)
                .map_err(unpack_error)?,
            Format::TarXz => {
                lzma_rs::xz_decompress(&mut &bytes[..], &mut tar).map_err(unpack_error)?
            }
            Format::Zst => ruzstd::decoding::StreamingDecoder::new(bytes)
                .map_err(|e| Error::Unpack(format!("not a zstd frame: {e}")))?
                .read_to_end(&mut tar)
                .map(drop)
                .map_err(unpack_error)?,
        }
        Ok(tar)
    }
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
    /// The download is an xz-compressed tar archive; the file is the regular file at
    /// `member` in it.
    TarXz {
        /// The path of the file inside the archive.
        member: PathBuf,
    },
    /// The download is one zstd-compressed file; the file is the download decompressed.
    Zst,
    /// The download is a tar archive packed as `format`; what is installed is the directory
    /// `top` in it, with every directory and regular file under it.
    Tree {
        /// How the archive is compressed.
        format: Format,
        /// The path of the directory inside the archive.
        top: PathBuf,
    },
}

/// What an [`Unpack`] makes of a download: one file, or a tree of them.
#[derive(Debug, PartialEq, Eq)]
enum Unpacked {
    /// The file to install at `--out`.
    File(Vec<u8>),
    /// The entries of the directory to install at `--out`, by their path under it, in the
    /// archive's order (a directory before what it holds, as tar writes them).
    Tree(Vec<(PathBuf, Node)>),
}

/// One entry of an [`Unpacked::Tree`].
#[derive(Debug, PartialEq, Eq)]
enum Node {
    /// A directory.
    Dir,
    /// A regular file, and whether the archive gives it any execute bit.
    File(Vec<u8>, bool),
}

impl Unpack {
    /// `--extract`, `--member` and `--tree` as given; `Err` names the flag that is missing
    /// or out of place.
    pub fn new(
        extract: Option<Format>,
        member: Option<PathBuf>,
        tree: bool,
    ) -> Result<Unpack, String> {
        match (extract, member, tree) {
            (None, None, false) => Ok(Unpack::Raw),
            (Some(Format::Zst), None, false) => Ok(Unpack::Zst),
            (Some(Format::Zst), _, _) => Err(
                "--extract zst is one compressed file: it takes no --member or --tree".to_owned(),
            ),
            (Some(Format::TarGz), Some(member), false) => Ok(Unpack::TarGz { member }),
            (Some(Format::TarXz), Some(member), false) => Ok(Unpack::TarXz { member }),
            (Some(format), Some(top), true) => Ok(Unpack::Tree { format, top }),
            (Some(_), None, _) => Err(
                "--extract needs --member, the file to take (with --tree, the directory)"
                    .to_owned(),
            ),
            (None, _, _) => Err(
                "--member and --tree name what to take out of a tar: they need --extract"
                    .to_owned(),
            ),
        }
    }

    /// What this unpacking makes of `bytes`.
    fn apply(&self, bytes: &[u8]) -> Result<Unpacked, Error> {
        match self {
            Unpack::Raw => Ok(Unpacked::File(bytes.to_vec())),
            Unpack::TarGz { member } => tar_member(&Format::TarGz.decode(bytes)?, member),
            Unpack::TarXz { member } => tar_member(&Format::TarXz.decode(bytes)?, member),
            Unpack::Zst => Ok(Unpacked::File(Format::Zst.decode(bytes)?)),
            Unpack::Tree { format, top } => tar_tree(&format.decode(bytes)?, top),
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
    /// The file was not in the cache, and no attempt in any round gave the pinned file.
    #[error(
        "{cache:?} was not in the actions cache, and no download was the pinned file: {}; nothing was installed",
        attempts.join("; ")
    )]
    NotCached {
        /// The `--cache` path, which held no file.
        cache: PathBuf,
        /// Each attempt, in order, on one line: its URL and why it failed.
        attempts: Vec<String>,
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
    /// The entry at the requested path is a directory, a link or another special file; or,
    /// in a tree, an entry under it is a link or another special file.
    #[error("{0:?} in the archive is not a regular file")]
    NotAFile(PathBuf),
    /// An entry of a tree whose path climbs out of the directory taken (a `..`).
    #[error("{0:?} in the archive climbs out of the directory taken")]
    Climbs(PathBuf),
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
    download_with(&agent(roots, timeout), url)
}

/// The client of the downloads: HTTPS only, trusting `roots` alone, each download (redirects
/// included) given up after `timeout`.
pub fn agent(roots: &[CertificateDer<'static>], timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .tls_config(tls_config(roots))
        .build()
        .into()
}

/// Downloads with one [`agent`] of `roots` and `timeout` for every attempt: each failure is
/// the [`Error::Download`] line, naming the URL.
pub fn https(
    roots: &[CertificateDer<'static>],
    timeout: Duration,
) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    let agent = agent(roots, timeout);
    move |url| download_with(&agent, url).map_err(|e| e.to_string())
}

/// The body of `url` downloaded by `agent`, at most [`MAX_DOWNLOAD`] bytes; a status other than
/// 2xx is an error.
fn download_with(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
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

/// What `unpack` makes of `bytes`, once their digest is `sha256`.
fn checked(bytes: &[u8], sha256: &Sha256, unpack: &Unpack) -> Result<Unpacked, Error> {
    let actual = Sha256::of(bytes);
    if actual != *sha256 {
        return Err(Error::Mismatch {
            expected: *sha256,
            actual,
        });
    }
    unpack.apply(bytes)
}

/// Writes `unpacked` to `out`: a file mode 0755, or a tree.
fn place(out: &Path, unpacked: &Unpacked) -> Result<(), Error> {
    match unpacked {
        Unpacked::File(file) => write_file(out, file, 0o755),
        Unpacked::Tree(nodes) => write_tree(out, nodes),
    }
    .map_err(|e| Error::Install {
        path: out.to_owned(),
        why: e.to_string(),
    })
}

/// [`download`]s `url` and [`install`]s it at `out`, in one attempt: the line to print.
pub fn fetch(
    url: &str,
    sha256: &Sha256,
    unpack: &Unpack,
    out: &Path,
    roots: &[CertificateDer<'static>],
) -> Result<String, Error> {
    let bytes = download(url, roots)?;
    install(&bytes, sha256, unpack, out)?;
    Ok(format!(
        "ok: {url} has SHA-256 {sha256}; installed {}",
        out.display()
    ))
}

/// The bytes of the first of `urls` whose download has SHA-256 `sha256`, round after round,
/// `pause` waiting the [`WAITS`] before each round, and the line saying where they came from:
/// `<url> has SHA-256 <hex>`, with the attempts that failed before it. `Err` is every attempt,
/// in order, as `<url>: <why>`: an error of `get` (on one line), or a download of other bytes.
pub fn first_pinned(
    urls: &[String],
    sha256: &Sha256,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<(Vec<u8>, String), Vec<String>> {
    let mut attempts = Vec::new();
    for wait in WAITS {
        pause(wait);
        for url in urls {
            let failed = match get(url) {
                Ok(bytes) if Sha256::of(&bytes) == *sha256 => {
                    let mut line = format!("{url} has SHA-256 {sha256}");
                    if !attempts.is_empty() {
                        line.push_str(&format!(
                            ", after {} failed: {}",
                            attempts.len(),
                            attempts.join("; ")
                        ));
                    }
                    return Ok((bytes, line));
                }
                Ok(bytes) => format!("SHA-256 {}, not the pinned one", Sha256::of(&bytes)),
                Err(why) => why.replace(['\r', '\n'], " "),
            };
            attempts.push(format!("{url}: {failed}"));
        }
    }
    Err(attempts)
}

/// What [`fetch_cached`] installs, and from where.
pub struct Cached<'a> {
    /// The URLs to download from on a miss, in the order they are tried.
    pub urls: &'a [String],
    /// The SHA-256 of the download (before unpacking).
    pub sha256: &'a Sha256,
    /// How the file comes out of the download.
    pub unpack: &'a Unpack,
    /// The file (with [`Unpack::Tree`], the directory) to install.
    pub out: &'a Path,
    /// The file the actions cache restores and saves: the download as it came.
    pub cache: &'a Path,
}

/// Installs `req.out` from the file at `req.cache` when it is there (checked, never replaced by a
/// download); when it is not, from the first of `req.urls` that gives the pinned bytes
/// ([`first_pinned`] with `get` and `pause`), kept at `req.cache` once it unpacked. The line to
/// print says which.
pub fn fetch_cached(
    req: &Cached<'_>,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<String, Error> {
    let cache = req.cache;
    let cache_error = |e: std::io::Error| Error::Cache {
        cache: cache.to_owned(),
        why: e.to_string(),
    };
    match std::fs::read(cache) {
        Ok(bytes) => {
            let file = checked(&bytes, req.sha256, req.unpack).map_err(|e| match e {
                Error::Mismatch { expected, actual } => Error::CachedMismatch {
                    cache: cache.to_owned(),
                    expected,
                    actual,
                },
                other => other,
            })?;
            place(req.out, &file)?;
            Ok(format!(
                "ok: the cached {} has SHA-256 {}; nothing was downloaded; installed {}",
                cache.display(),
                req.sha256,
                req.out.display()
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let (bytes, from) =
                first_pinned(req.urls, req.sha256, get, pause).map_err(|attempts| {
                    Error::NotCached {
                        cache: cache.to_owned(),
                        attempts,
                    }
                })?;
            let file = checked(&bytes, req.sha256, req.unpack)?;
            write_file(cache, &bytes, 0o644).map_err(cache_error)?;
            place(req.out, &file)?;
            Ok(format!(
                "ok: {from}; kept it at {} and installed {}",
                cache.display(),
                req.out.display()
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

/// Writes the tree `nodes` as the directory `<out>.part` and renames it to `out`, removing
/// the part directory when a step fails. A part directory a killed run left is removed
/// first.
fn write_tree(out: &Path, nodes: &[(PathBuf, Node)]) -> std::io::Result<()> {
    let part = part_path(out);
    // Absent unless a run was killed while writing it.
    let _ = std::fs::remove_dir_all(&part);
    let written = std::fs::create_dir_all(&part)
        .and_then(|()| {
            nodes
                .iter()
                .try_for_each(|(path, node)| write_node(&part.join(path), node))
        })
        .and_then(|()| std::fs::rename(&part, out));
    if written.is_err() {
        // A part directory that cannot be removed is reported by the write error itself.
        let _ = std::fs::remove_dir_all(&part);
    }
    written
}

/// Writes one entry of a tree at `path`, making its directories when the archive did not
/// list them first.
fn write_node(path: &Path, node: &Node) -> std::io::Result<()> {
    match node {
        Node::Dir => std::fs::create_dir_all(path),
        Node::File(data, executable) => {
            let mode = if *executable { 0o755 } else { 0o644 };
            path.parent().map_or(Ok(()), std::fs::create_dir_all)?;
            std::fs::write(path, data).and_then(|()| {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            })
        }
    }
}

/// The directory `top` of the tar archive `tar`: every directory and regular file under it,
/// by its path under `top`. Entries elsewhere in the archive are passed over.
fn tar_tree(tar: &[u8], top: &Path) -> Result<Unpacked, Error> {
    let mut archive = tar::Archive::new(tar);
    let mut nodes = Vec::new();
    let mut found = false;
    for entry in archive.entries().map_err(unpack_error)? {
        let mut entry = entry.map_err(unpack_error)?;
        let path = entry.path().map_err(unpack_error)?.into_owned();
        let Ok(under) = path.strip_prefix(top) else {
            continue;
        };
        found = true;
        if under
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(Error::Climbs(path));
        }
        let kind = entry.header().entry_type();
        let node = if kind.is_dir() {
            Node::Dir
        } else if kind.is_file() {
            let executable = entry.header().mode().map_err(unpack_error)? & 0o111 != 0;
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(unpack_error)?;
            Node::File(data, executable)
        } else {
            return Err(Error::NotAFile(path));
        };
        nodes.push((under.to_owned(), node));
    }
    if found {
        Ok(Unpacked::Tree(nodes))
    } else {
        Err(Error::NoMember(top.to_owned()))
    }
}

/// The contents of the regular file at `member` in the tar archive `tar`.
fn tar_member(tar: &[u8], member: &Path) -> Result<Unpacked, Error> {
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
        return Ok(Unpacked::File(file));
    }
    Err(Error::NoMember(member.to_owned()))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cache_tests;
