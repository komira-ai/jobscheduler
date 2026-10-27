//! The per-run scratch directories, below one root only their owner may enter.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The directory under which every run gets a scratch directory of its own, `0700`.
///
/// The root must be a directory (not a link to one) that the user running the agent
/// owns, with no permission for group or others: a scratch directory is only as private
/// as the directory above it. The agent checks that when it starts and does not change
/// the root. The path is kept in its canonical form (every link on the way resolved), so
/// the path a run's directory is bound by is the one the engine reports for it when the
/// mounts are read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRoot {
    path: PathBuf,
}

impl ScratchRoot {
    /// Opens the root at `path`.
    ///
    /// # Errors
    ///
    /// The path is not an existing directory, is a link, is owned by another user, or
    /// grants group or others any permission. The text names the flag.
    pub fn open(path: &Path) -> Result<Self, String> {
        let flag = |why: &str| format!("--scratch-root: {why}");
        let (meta, canonical) = fs::symlink_metadata(path)
            .and_then(|meta| fs::canonicalize(path).map(|canonical| (meta, canonical)))
            .map_err(|_| flag("cannot read the directory"))?;
        let mine = meta.uid() == rustix::process::geteuid().as_raw();
        let owner_only = meta.mode() & 0o077 == 0;
        match (meta.is_dir(), mine, owner_only) {
            (false, _, _) => Err(flag("not a directory (a link is not accepted)")),
            (true, false, _) => Err(flag("not owned by the user running the agent")),
            (true, true, false) => Err(flag("group and others must have no permission")),
            (true, true, true) => Ok(Self { path: canonical }),
        }
    }

    /// The directory of the run, which need not exist.
    pub fn of(&self, run_id: &str) -> PathBuf {
        self.path.join(run_id)
    }

    /// Makes the run's directory, mode `0700` whatever the umask. A directory left by an
    /// earlier agent for this run id is removed first: the run is new, so it is garbage.
    ///
    /// # Errors
    ///
    /// The stale directory cannot be removed or the new one cannot be made.
    pub fn create(&self, run_id: &str) -> io::Result<PathBuf> {
        self.remove(run_id)?;
        let dir = self.of(run_id);
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    /// Removes the run's directory and everything in it; one that is not there is not an
    /// error. Links inside are removed, never followed.
    ///
    /// # Errors
    ///
    /// Something in it cannot be removed.
    pub fn remove(&self, run_id: &str) -> io::Result<()> {
        match fs::remove_dir_all(self.of(run_id)) {
            Err(why) if why.kind() != io::ErrorKind::NotFound => Err(why),
            _ => Ok(()),
        }
    }
}
