//! The per-run files directories (`JobSpec::files`), kept on a `tmpfs` so a presigned URL
//! or a token is never written to a disk.
//!
//! Each run gets `<root>/<run_id>`, `0555` with its files `0444`: the container's user
//! can read them (the directory is bind-mounted read-only at `/run/js-files`), and nobody
//! can add, change or unlink one without first changing the directory's mode. The root is
//! `0700` and the agent's.
//!
//! A root opened with [`DirLayout::Searchable`] (the rootless crun runtime) is `0711`, so
//! the job's ids can walk down to a run's directory, and the run's directory is `0700` with
//! its files `0400`, the agent's, until [`FilesRoot::hand_over`] gives them to the job's
//! ids and closes the directory to `0500`: a job that was not handed its files, and every
//! other user, reads nothing. A reboot clears a tmpfs, which is fine: a run does not survive
//! a reboot, and the agent's start-up sweep would remove what an earlier process left in
//! it anyway (except the directories of runs it adopts). A tmpfs can be swapped to disk when
//! the host has swap, so a host that handles these secrets runs with swap off or encrypted.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use js_agent_wire::RunFile;

use super::scratch::{DirLayout, layout_dir, remove_entry, sweep_entries};

/// Where `--run-files-root` points when the flag is not given.
pub const DEFAULT_ROOT: &str = "/run/js-agent/files";

/// The directory the runs' files directories are made in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesRoot {
    path: PathBuf,
    layout: DirLayout,
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
        Self::open_with(path, DirLayout::Private)
    }

    /// [`FilesRoot::open`] with the permissions `layout` allows; a missing root is made
    /// with the mode of the layout (`0700`, or `0711` for [`DirLayout::Searchable`]).
    ///
    /// # Errors
    ///
    /// As [`FilesRoot::open`]; for [`DirLayout::Searchable`] also an ancestor of the root
    /// that others cannot search.
    pub fn open_with(path: &Path, layout: DirLayout) -> Result<Self, String> {
        let missing = fs::symlink_metadata(path).is_err();
        if missing {
            let made = fs::DirBuilder::new()
                .recursive(true)
                .mode(layout.dir_mode())
                .create(path)
                .and_then(|()| {
                    fs::set_permissions(path, fs::Permissions::from_mode(layout.dir_mode()))
                });
            made.map_err(|_| "--run-files-root: cannot make the directory".to_owned())?;
        }
        layout_dir(path, "--run-files-root", layout).map(|path| Self { path, layout })
    }

    /// The layout the root was opened with.
    pub const fn layout(&self) -> DirLayout {
        self.layout
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
    /// In a [`DirLayout::Searchable`] root the files are `0400` and the directory stays
    /// `0700` and the agent's until [`FilesRoot::hand_over`].
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
        let handed = self.layout == DirLayout::Searchable;
        let file_mode = if handed { 0o400 } else { 0o444 };
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
                .mode(file_mode)
                .open(dir.join(&file.name))?;
            out.write_all(file.contents.as_bytes())?;
            // The umask can only take bits away, but a restrictive one would take read
            // from the container's user.
            out.set_permissions(fs::Permissions::from_mode(file_mode))?;
        }
        if handed {
            return Ok(());
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
    }

    /// Gives the run's files directory and its files to the job's ids (`uid`, `gid` are host
    /// ids) and closes the directory to `0500`, so the job reads its files and nobody else
    /// does. Only for a [`DirLayout::Searchable`] root, whose directories are `0700` and the
    /// agent's until now. It needs the privilege to give files away (root, or
    /// `CAP_CHOWN`); an agent without it hands the directory over through the container
    /// runner's helper instead.
    ///
    /// The calls are by path (`lchown` on each entry, which does not follow a link in the
    /// last component, then `chmod` and `lchown` of the directory), and that is safe because
    /// until the final `lchown` the directory and its root are the agent's and `0700`/`0711`:
    /// no other user can swap a component for a link. Once it is the job's, the agent only
    /// removes it, which walks by descriptor and never follows a link.
    ///
    /// # Errors
    ///
    /// The root is not a [`DirLayout::Searchable`] one, the run id is not one ordinary path
    /// component, the directory is missing, or a change of owner or mode failed.
    pub fn hand_over(&self, run_id: &str, uid: u32, gid: u32) -> io::Result<()> {
        use std::os::unix::fs::lchown;
        if self.layout != DirLayout::Searchable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the files of a private root are already readable by the job",
            ));
        }
        let plain = !matches!(run_id, "" | "." | "..") && !run_id.contains(['/', '\0']);
        if !plain {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let dir = self.of(run_id);
        for entry in fs::read_dir(&dir)? {
            lchown(entry?.path(), Some(uid), Some(gid))?;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500))?;
        lchown(&dir, Some(uid), Some(gid))
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
