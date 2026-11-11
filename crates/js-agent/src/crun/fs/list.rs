//! The list of a run's mounts: the file `<mounts>/<lease>/mounts`, one mount point a line,
//! each written before its mount is made, so whatever ends the run (its removal, a failed
//! assembly, a restart that finds it) unmounts every mount the run may have, from the file
//! alone. A point listed whose mount was never made is skipped by the unmount. The file is
//! the runtime's own; the ledger's sidecars, which an agent of an earlier release reads
//! strictly, do not change.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use js_contain::job_fs::{MountCalls, unmount_all};

/// The name of the list in a run's directory of mounts.
pub(super) const LIST: &str = "mounts";

/// Makes the directory `dir` (and any missing directory above it), mode `0711`: root's,
/// and searchable by the job user, whose `crun` binds what lies below it.
///
/// # Errors
/// The directory could not be made.
pub(super) fn make_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o711)
        .create(dir)
}

/// Makes the directory `dir` (and any missing directory above it), and sets it and its
/// parent to mode `0711` whatever the umask: the job user's `crun` must search down to the
/// clones it binds.
///
/// # Errors
/// A directory could not be made or its mode set.
pub(super) fn make_searchable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    make_dir(dir)?;
    dir.parent()
        .into_iter()
        .chain([dir])
        .try_for_each(|each| std::fs::set_permissions(each, std::fs::Permissions::from_mode(0o711)))
}

/// Adds `point` to the list of the run whose directory of mounts is `dir`, and makes the
/// directory `point`; the line is on disk before the call returns, so before the mount is
/// made.
///
/// # Errors
/// The list could not be written or the point made.
pub(super) fn note(dir: &Path, point: &Path) -> io::Result<()> {
    let mut list = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LIST))?;
    writeln!(list, "{}", point.display())?;
    list.sync_data()?;
    make_searchable(point)
}

/// The points the list of `dir` names, in the order they were added; none when there is
/// no list.
///
/// # Errors
/// The list is there and could not be read.
pub(super) fn points(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match std::fs::read_to_string(dir.join(LIST)) {
        Ok(text) => Ok(text.lines().map(PathBuf::from).collect()),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(why) => Err(why),
    }
}

/// Unmounts every point the list of `dir` names, deepest first, and removes `dir` with what
/// is left in it (the list, the empty points). A `dir` that is not there has nothing to
/// remove.
///
/// # Errors
/// The list could not be read, a point could not be unmounted (`dir` stays, and with it
/// the list, for the next try), or `dir` could not be removed.
pub(super) fn release(calls: &dyn MountCalls, dir: &Path) -> Result<(), String> {
    let listed = points(dir).map_err(|why| format!("read the list of {}: {why}", dir.display()))?;
    unmount_all(calls, &listed).map_err(|why| why.to_string())?;
    match js_contain::remove::remove_tree(dir) {
        Err(why) if why.kind() != io::ErrorKind::NotFound => {
            Err(format!("remove {}: {why}", dir.display()))
        }
        _ => Ok(()),
    }
}
