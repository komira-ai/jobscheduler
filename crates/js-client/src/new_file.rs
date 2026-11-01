//! Making a file that must not already exist, with its whole content or not at all.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Makes the file `path` with `bytes` in it and permission bits `mode`, if nothing is at
/// `path` yet. The bytes are written to a file beside it under a name made for this call
/// (`.<name>.<16 hex digits>.new`, made with `O_EXCL`, so a name another user planted is
/// an error and not a file written through), synced, and hard-linked to `path`; the link
/// fails if anything is there, a dangling symbolic link included, so no reader ever sees a
/// part of the content and an existing file is never replaced. The staged name is removed
/// whether or not the link was made. The staged file is made with `mode` (the umask can only
/// take bits off it), so it is never more open than the file will be.
///
/// Once the link is made the call has succeeded: the sync of the directory that follows is
/// best effort, because a directory that cannot be read cannot be synced and the file is in
/// place all the same.
///
/// # Errors
///
/// Any step up to the link failed; nothing is then at `path` that this call made. The error
/// is [`io::ErrorKind::AlreadyExists`] when something is already at `path`. The text of an
/// error is the system's and never holds `bytes`. A crash between the staging and the
/// removal leaves the staged file, under its own name.
pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let staged = staging_name(path)?;
    let made = stage(&staged, bytes, mode).and_then(|()| fs::hard_link(&staged, path));
    let _ = fs::remove_file(&staged);
    made.inspect(|()| {
        let _ = sync_parent(path);
    })
}

/// Whether a file can be made in `dir` and linked to another name there, which is all
/// [`create_new`] asks of a directory: found by doing it, under names made for the probe, and
/// removing them. What it cannot tell is a failure later (a full disk, a directory changed in
/// between).
///
/// # Errors
///
/// The system's reason the directory cannot take [`create_new`].
pub fn probe_dir(dir: &Path) -> io::Result<()> {
    let (first, second) = (dir.join(unique(".probe")?), dir.join(unique(".probe")?));
    let made = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&first)
        .and_then(|_| fs::hard_link(&first, &second));
    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    made
}

/// `<prefix>.<16 random hex digits>`.
fn unique(prefix: &str) -> io::Result<String> {
    let mut bits = [0u8; 8];
    getrandom::getrandom(&mut bits).map_err(io::Error::from)?;
    Ok(format!("{prefix}.{:016x}", u64::from_le_bytes(bits)))
}

/// `<dir>/.<name>.<16 hex digits>.new`.
fn staging_name(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(path.with_file_name(unique(&format!(".{name}"))? + ".new"))
}

/// Writes `bytes` to `staged`, which is made new, and syncs it.
fn stage(staged: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
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
