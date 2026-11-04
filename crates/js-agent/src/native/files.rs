//! The run files of a native run: the directory the lifecycle wrote a start's `files` into
//! (`RunSpec::files`, in the `--run-files-root` of a native agent, which is searchable), given
//! to the run's user once that user exists and before its program starts.
//!
//! Until then the directory is the agent's and `0700`, its files `0400`, and no process of
//! the run's user exists, so nothing can swap a name the calls below resolve. Afterwards the
//! directory is `0500` and its files `0400`, the user's: the run reads them, and every other
//! user but root reads nothing. The run's profile denies writes there, and `JS_FILES_DIR` in
//! the run's environment names the directory (the `JS_` prefix is the agent's, so a job
//! cannot set it).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;

/// The variable that names a run's files directory.
pub(super) const FILES_DIR_ENV: &str = "JS_FILES_DIR";

/// Gives one path to a user and a group (`uid`, `gid`), not following a link in its last
/// component. The agent's is `lchown`, which needs root; a test passes its own.
pub type HandOver = Arc<dyn Fn(&Path, u32, u32) -> io::Result<()> + Send + Sync>;

/// The [`HandOver`] of an agent: `lchown`.
pub(super) fn lchown() -> HandOver {
    Arc::new(|path, uid, gid| std::os::unix::fs::lchown(path, Some(uid), Some(gid)))
}

/// Gives the directory `dir` and every file in it to `uid` through `chown`, the files `0400`
/// and the directory `0500`. The group stays the directory's (the modes give it nothing).
///
/// # Errors
///
/// The directory cannot be read, or a mode or an owner cannot be changed.
pub(super) fn hand_over(dir: &Path, uid: u32, chown: &HandOver) -> io::Result<()> {
    let gid = fs::symlink_metadata(dir)?.gid();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
        chown(&path, uid, gid)?;
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500))?;
    chown(dir, uid, gid)
}
