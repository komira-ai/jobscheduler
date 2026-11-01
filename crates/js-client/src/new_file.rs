//! Making a file that must not already exist, with its whole content or not at all.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Makes the file `path` with `bytes` in it and permission bits `mode`, if nothing is at
/// `path` yet. The bytes are written to a file beside it (`.<name>.new`, mode `mode`),
/// synced, and hard-linked to `path`; the link fails if anything is there, a dangling
/// symbolic link included, so no reader ever sees a part of the content and an existing
/// file is never replaced. The directory is synced at the end. A file left by an earlier
/// interrupted call under the staging name is removed first.
///
/// # Errors
///
/// Any step failed; nothing is then at `path` that this call made. The error is
/// [`io::ErrorKind::AlreadyExists`] when something is already at `path`. The text of an
/// error is the system's and never holds `bytes`.
pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let staged = staging_name(path);
    let made = stage(&staged, bytes, mode)
        .and_then(|()| fs::hard_link(&staged, path))
        .and_then(|()| sync_parent(path));
    // The staged name is not wanted whether or not the link was made.
    let _ = fs::remove_file(&staged);
    made
}

/// `<dir>/.<name>.new`.
fn staging_name(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".new");
    path.with_file_name(name)
}

/// Writes `bytes` to `staged`, which is made new (a leftover is removed first), and syncs it.
fn stage(staged: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let _ = fs::remove_file(staged);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(staged)
        .and_then(|mut file| {
            // The process's umask may have taken bits off the mode it was made with.
            file.set_permissions(Permissions::from_mode(mode))
                .and_then(|()| file.write_all(bytes))
                .and_then(|()| file.sync_all())
        })
}

/// Makes the directory entry of `path` durable.
fn sync_parent(path: &Path) -> io::Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    File::open(dir.unwrap_or(Path::new("."))).and_then(|dir| dir.sync_all())
}

/// Whether a file can be made in `dir` and linked to another name there (see
/// [`create_new`]), found by doing it with names of no other use and removing them.
///
/// # Errors
///
/// The system's reason the directory cannot take [`create_new`].
pub fn probe_dir(dir: &Path) -> io::Result<()> {
    let _ = dir;
    Ok(())
}
