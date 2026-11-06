//! Reading the secret files: a file is read only when it is a regular file owned by the
//! user running the server that nobody else can read or write, and a refusal names the path,
//! never the content.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::fs::OFlags;
use rustix::process::geteuid;

use crate::error::StartupError;

/// Whether a secret file may be trusted: a regular file, owned by the user running the
/// server (`euid`), that nobody else can read or write (mode 0600 or 0400).
fn trusted_private_file(is_file: bool, mode: u32, owner: u32, euid: u32) -> bool {
    is_file & matches!(mode, 0o600 | 0o400) & (owner == euid)
}

/// Reads the secret file at `path`, named by `flag`, after checking, on the open file,
/// that it is a regular file owned by the server's user that only that user can use.
/// See [`read_private_reason`].
pub(crate) fn read_private(path: &Path, flag: &'static str) -> Result<Vec<u8>, StartupError> {
    read_private_reason(path).map_err(StartupError::because(flag))
}

/// Reads the secret file at `path` after checking, on the open file, that it is a regular
/// file owned by the server's user that only that user can use, and says why it was not
/// read otherwise. The file is opened without blocking, so a named pipe at the path is
/// refused instead of waited on; a symbolic link is followed (secret mounts are links)
/// and the checks are on what it leads to. The reason names the path and never the
/// content.
pub(crate) fn read_private_reason(path: &Path) -> Result<Vec<u8>, String> {
    match open_private(path) {
        Ok(read) => read,
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// [`read_private_reason`] with the system's failures on the outside: a file that could
/// not be opened, inspected or read, and inside it the refusal a file earns by what it
/// is.
fn open_private(path: &Path) -> std::io::Result<Result<Vec<u8>, String>> {
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path)?;
    let meta = file.metadata()?;
    let mode = meta.permissions().mode() & 0o777;
    let euid = geteuid().as_raw();
    if !trusted_private_file(meta.is_file(), mode, meta.uid(), euid) {
        return Ok(Err(format!(
            "{} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode {mode:04o} owned by uid {}",
            path.display(),
            meta.uid()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Ok(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a token file is trusted only when it is a regular file, owner-only
    /// in mode, and owned by the user running the server: each condition alone refuses.
    #[test]
    fn a_token_file_must_be_the_servers_own_private_file() {
        assert!(trusted_private_file(true, 0o600, 1000, 1000));
        assert!(trusted_private_file(true, 0o400, 1000, 1000));
        assert!(!trusted_private_file(false, 0o600, 1000, 1000));
        assert!(!trusted_private_file(true, 0o640, 1000, 1000));
        assert!(!trusted_private_file(true, 0o600, 0, 1000));
        assert!(!trusted_private_file(true, 0o600, 1000, 0));
    }
}
