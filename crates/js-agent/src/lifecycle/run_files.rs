//! The per-run files directories (`JobSpec::files`), kept on a `tmpfs` so a presigned URL
//! or a token is never written to a disk.
//!
//! Each run gets `<root>/<run_id>`, `0555` with its files `0444`: the container's user
//! can read them (the directory is bind-mounted read-only at `/run/js-files`), and nobody
//! can add, change or unlink one without first changing the directory's mode. The root is
//! `0700` and the agent's. A reboot clears a tmpfs, which is fine: a run does not survive
//! a reboot, and the agent's start-up sweep would remove what an earlier process left in
//! it anyway (except the directories of runs it adopts). A tmpfs can be swapped to disk when
//! the host has swap, so a host that handles these secrets runs with swap off or encrypted.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use js_agent_wire::RunFile;

use super::scratch::{private_dir, remove_entry, sweep_entries};

/// Where `--run-files-root` points when the flag is not given.
pub const DEFAULT_ROOT: &str = "/run/js-agent/files";

/// The directory the runs' files directories are made in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesRoot {
    path: PathBuf,
}

impl FilesRoot {
    /// Opens the root at `path`, making it (and the directories above it) `0700` when it
    /// is missing. Whether it is on a `tmpfs` is not decided here: the agent asks the file
    /// system through its `FileReader` seam ([`FilesRoot::path`]) at start.
    ///
    /// # Errors
    ///
    /// The directory cannot be made, or is not a directory (not a link) that the user
    /// running the agent owns with no permission for group or others. The text names the
    /// flag.
    pub fn open(path: &Path) -> Result<Self, String> {
        let missing = fs::symlink_metadata(path).is_err();
        if missing {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| "--run-files-root: cannot make the directory".to_owned())?;
        }
        private_dir(path, "--run-files-root").map(|path| Self { path })
    }

    /// The canonical path of the root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory of the run's files, which need not exist.
    pub fn of(&self, run_id: &str) -> PathBuf {
        self.path.join(run_id)
    }

    /// Writes the run's files into a new directory and returns it. A directory left for
    /// this run id is removed first. Each file is created `0444` and must not exist
    /// (`O_EXCL`), then the directory is made `0555`; it is `0700` while it is filled.
    ///
    /// # Errors
    ///
    /// A stale directory that cannot be removed, a name that is not one ordinary path
    /// component, a file that already exists, or a write that fails. The directory is
    /// removed again before the error is returned, so a failed start leaves no secret on
    /// the file system. The error never holds a name or any contents.
    pub fn write(&self, run_id: &str, files: &[RunFile]) -> io::Result<PathBuf> {
        remove_entry(&self.path, run_id)?;
        let dir = self.of(run_id);
        self.fill(&dir, files).map(|()| dir).inspect_err(|_| {
            let _ = remove_entry(&self.path, run_id);
        })
    }

    fn fill(&self, dir: &Path, files: &[RunFile]) -> io::Result<()> {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        for file in files {
            let plain =
                !matches!(file.name.as_str(), "" | "." | "..") && !file.name.contains(['/', '\0']);
            if !plain {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o444)
                .open(dir.join(&file.name))?;
            out.write_all(file.contents.as_bytes())?;
            // The umask can only take bits away, but a restrictive one would take read
            // from the container's user.
            out.set_permissions(fs::Permissions::from_mode(0o444))?;
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
    }

    /// Removes the run's directory and its files, whatever their mode; one that is not
    /// there is not an error.
    ///
    /// # Errors
    ///
    /// The run id is not one ordinary path component, or something cannot be removed.
    pub fn remove(&self, run_id: &str) -> io::Result<()> {
        remove_entry(&self.path, run_id)
    }

    /// Removes every directory of the root that is not a run in `kept`, and returns how
    /// many it removed. For the start of the agent: what an earlier process left for a run
    /// that no longer has a container is garbage, and a kept run keeps its files.
    ///
    /// # Errors
    ///
    /// The root cannot be listed. An entry that cannot be removed stays and is not counted.
    pub fn sweep<'a>(&self, kept: impl IntoIterator<Item = &'a str>) -> io::Result<usize> {
        let kept: Vec<&str> = kept.into_iter().collect();
        let keep = |name: &[u8]| kept.iter().any(|run| run.as_bytes() == name);
        sweep_entries(&self.path, &keep)
    }
}
