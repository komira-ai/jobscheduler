//! Making a file that must not already exist, with its whole content or not at all.

use std::io;
use std::path::Path;

/// Makes the file `path` with `bytes` in it and permission bits `mode`, if nothing is at
/// `path` yet. The bytes are written to a file beside it, synced, and hard-linked to
/// `path`; the link fails if anything is there, so no reader ever sees a part of the
/// content and an existing file is never replaced.
///
/// # Errors
///
/// Any step failed; nothing is then at `path` that this call made. The error is
/// [`io::ErrorKind::AlreadyExists`] when something is already at `path`. The text of an
/// error is the system's and never holds `bytes`.
pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let _ = (path, bytes, mode);
    todo!("create_new")
}
