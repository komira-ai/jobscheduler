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
//!
//! The inputs of a native run are staged the same way, into `exec/`: each `mount` input,
//! which the lifecycle resolved below one of the agent's mount roots (`RunSpec::mounts`, its
//! `target` the input's `dest`), is copied to `exec/<dest>` while no process of the user
//! exists, every directory `0500` and every file `0400` (`0500` when the source is
//! executable), given to the user; a link is copied
//! as the link it is. The profile denies writes below each `dest`, as it does below the files
//! directory, so the job reads its inputs and cannot change them. A part of `dest` above the
//! input that is not there is made `0700`, the user's, as `exec/` is.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::lifecycle::ResolvedMount;

/// The variable that names a run's files directory.
pub(super) const FILES_DIR_ENV: &str = "JS_FILES_DIR";

/// Gives one path to a user and a group (`uid`, `gid`), not following a link in its last
/// component. The agent's is `lchown`, which needs root; a test passes its own.
pub type HandOver = Arc<dyn Fn(&Path, u32, u32) -> io::Result<()> + Send + Sync>;

/// Makes one missing directory of a run, as the run's user's.
pub(super) type MakeDir<'a> = &'a dyn Fn(&Path) -> io::Result<()>;

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

/// Where the input `input` (its `target` the input's `dest`) is staged below `exec`.
pub(super) fn input_dest(exec: &Path, input: &ResolvedMount) -> PathBuf {
    exec.join(&input.target)
}

/// Copies each input of `inputs` from its host path to its `dest` below `exec` and gives the
/// copy to `uid` (see the module documentation). `make` makes a missing part of `dest`
/// above the input, as the user's.
///
/// # Errors
///
/// The `dest` of an input exists already, or an input cannot be read, copied or given away;
/// the text names the input.
pub(super) fn stage_inputs(
    exec: &Path,
    inputs: &[ResolvedMount],
    uid: u32,
    (chown, make): (&HandOver, MakeDir<'_>),
) -> io::Result<()> {
    let gid = fs::symlink_metadata(exec)?.gid();
    for input in inputs {
        let dest = input_dest(exec, input);
        let named = |why: io::Error| io::Error::new(why.kind(), format!("{}: {why}", input.target));
        let above = Path::new(&input.target).parent().unwrap_or(Path::new(""));
        make_parents(exec, above, make).map_err(named)?;
        copy_tree(&input.host, &dest, (uid, gid), chown).map_err(named)?;
    }
    Ok(())
}

/// Makes every directory of `below` (a path relative to `exec`, which exists) that is not
/// there.
fn make_parents(exec: &Path, below: &Path, make: MakeDir<'_>) -> io::Result<()> {
    let mut at = exec.to_path_buf();
    for part in below.components() {
        at.push(part);
        if fs::symlink_metadata(&at).is_err() {
            make(&at)?;
        }
    }
    Ok(())
}

/// Copies `from` to the new path `to` (a directory with everything below it, a file, or a
/// link as the link it is), never following a link, and gives each copy to `owner`.
fn copy_tree(from: &Path, to: &Path, owner: (u32, u32), chown: &HandOver) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if kind.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let name = entry?.file_name();
            copy_tree(&from.join(&name), &to.join(&name), owner, chown)?;
        }
        fs::set_permissions(to, fs::Permissions::from_mode(0o500))?;
    } else if kind.is_file() {
        // A new file: `copy` would write through a link or into a file already at `to`.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        fs::copy(from, to)?;
        // Read-only, and executable by its user when the source is by anyone.
        let mode = match meta.permissions().mode() & 0o111 {
            0 => 0o400,
            _ => 0o500,
        };
        fs::set_permissions(to, fs::Permissions::from_mode(mode))?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a file, a directory or a link", from.display()),
        ));
    }
    chown(to, owner.0, owner.1)
}
