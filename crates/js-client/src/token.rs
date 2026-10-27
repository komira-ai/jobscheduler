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
    let probe = probe(path).map_err(|err| refuse(format!("cannot be read: {err}")))?;
    if !trusted(&probe, geteuid().as_raw()) {
        return Err(refuse(format!(
            "must be a regular file owned by you and readable only by you (mode 0600 or 0400), not mode {:04o} owned by uid {}",
            probe.mode, probe.owner
        )));
    }
    let token = token(&probe.bytes).map_err(refuse)?;
    Ok(format!("Bearer {token}"))
}

/// Opens `path` without blocking, so a named pipe is refused instead of waited on (a
/// symbolic link is followed: secret mounts are links), and reads it only when the open
/// file passes the checks.
fn probe(path: &Path) -> std::io::Result<Probe> {
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
                    .take(MAX_TOKEN_FILE)
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
