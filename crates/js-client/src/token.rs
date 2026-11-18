//! The bearer token: read from a file that only its owner can use, checked as the server
//! checks its own.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::fs::OFlags;
use rustix::process::geteuid;

use crate::error::ClientError;

/// The most a token file may hold; a token is a short line.
const MAX_TOKEN_FILE: u64 = 4096;

/// What opening a token file showed.
struct Probe {
    is_file: bool,
    mode: u32,
    owner: u32,
    bytes: Vec<u8>,
}

/// The `Authorization` header value for the token in `path`. The file must be a regular
/// file owned by the user running `js` that nobody else can read or write (mode 0600 or
/// 0400), and hold one token of printable ASCII (trailing whitespace is dropped).
///
/// # Errors
///
/// [`ClientError::Flag`] naming `--token-file` when the file cannot be read, is not
/// owner-only, or does not hold a token.
pub fn bearer(path: &Path) -> Result<String, ClientError> {
    let refuse =
        |why: String| ClientError::flag("--token-file", format!("{} {why}", path.display()));
    let bytes = owner_only(path, MAX_TOKEN_FILE).map_err(refuse)?;
    let token = token(&bytes).map_err(refuse)?;
    Ok(format!("Bearer {token}"))
}

/// The first `max` bytes of the file at `path`, when it is a regular file owned by the
/// user running the program that nobody else can read or write (mode 0600 or 0400). The
/// text of an error follows the path in a message and never holds the file's content.
pub(crate) fn owner_only(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let probe = probe(path, max).map_err(|err| format!("cannot be read: {err}"))?;
    match trusted(&probe, geteuid().as_raw()) {
        true => Ok(probe.bytes),
        false => Err(format!(
            "must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode {:04o} owned by uid {}",
            probe.mode, probe.owner
        )),
    }
}

/// The `Authorization` header value for `secret`, a credential the caller holds in memory
/// (the host credential a welcome carried). It must be one run of printable ASCII, as the
/// content of a token file must.
///
/// # Errors
///
/// [`ClientError::Flag`] naming `--token-file`, the flag the bearer is otherwise read for,
/// when `secret` is empty or holds a space, a control character or a byte above 127. The
/// text says what is wrong and never holds the secret.
pub fn bearer_of(secret: &str) -> Result<String, ClientError> {
    token(secret.as_bytes())
        .map(|token| format!("Bearer {token}"))
        .map_err(|why| ClientError::flag("--token-file", format!("the credential {why}")))
}

/// Opens `path` without blocking, so a named pipe is refused instead of waited on (a
/// symbolic link is followed: secret mounts are links), and reads it only when the open
/// file passes the checks.
fn probe(path: &Path, max: u64) -> std::io::Result<Probe> {
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path);
    opened.and_then(|mut file| {
        file.metadata().and_then(|meta| {
            let mut probe = Probe {
                is_file: meta.is_file(),
                mode: meta.permissions().mode() & 0o777,
                owner: meta.uid(),
                bytes: Vec::new(),
            };
            match trusted(&probe, geteuid().as_raw()) {
                true => (&mut file)
                    .take(max)
                    .read_to_end(&mut probe.bytes)
                    .map(|_| probe),
                false => Ok(probe),
            }
        })
    })
}

/// Whether the file may be trusted: regular, owned by `euid`, mode 0600 or 0400.
fn trusted(probe: &Probe, euid: u32) -> bool {
    probe.is_file & matches!(probe.mode, 0o600 | 0o400) & (probe.owner == euid)
}

/// The token a file's bytes hold: the content without trailing whitespace, which must be
/// one run of printable ASCII (a header value can carry nothing else).
fn token(bytes: &[u8]) -> Result<&str, String> {
    let token = bytes.trim_ascii_end();
    match (token.is_empty(), token.iter().all(u8::is_ascii_graphic)) {
        (true, _) => Err("is empty".to_owned()),
        (false, false) => Err(
            "must hold one token of printable ASCII with no space or line break inside it"
                .to_owned(),
        ),
        (false, true) => Ok(std::str::from_utf8(token).unwrap_or_default()),
    }
}
