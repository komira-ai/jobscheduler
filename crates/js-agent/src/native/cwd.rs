//! The working directory of a native run: `native.cwd`, a path relative to the run's
//! `exec/` directory (empty for `exec/` itself), resolved there and handed to the helper as
//! the open directory its `run` request carries.
//!
//! A path that leaves `exec/` is refused, by name, before anything starts: an absolute path
//! or one with a `..` part at once (also at admission, [`check`]), and one that reaches
//! outside through a link (an input may carry links) when it is resolved, after the inputs
//! are staged and before the helper's `run`. No process of the run's user exists then, so
//! nothing can swap a name between the resolution and the open. A part of the path that is
//! not there is made, `0700` and the user's, as `exec/` is.

use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Why a working directory cannot be used.
#[derive(Debug, thiserror::Error)]
pub(super) enum CwdError {
    /// The path leaves `exec/`: it is absolute, has a `..` part, or a link in it resolves
    /// outside.
    #[error("native.cwd {0:?} escapes the run directory")]
    Escapes(String),
    /// A part of the path is not a directory, or cannot be made or opened.
    #[error("native.cwd {0:?}: {1}")]
    Unusable(String, io::Error),
}

/// Refuses a working directory that leaves `exec/` by its text alone: an absolute path or a
/// `..` part. The empty path is `exec/` itself.
///
/// # Errors
///
/// [`CwdError::Escapes`].
pub(super) fn check(cwd: &str) -> Result<(), CwdError> {
    let lexical = Path::new(cwd)
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    match lexical {
        true => Ok(()),
        false => Err(CwdError::Escapes(cwd.to_owned())),
    }
}

/// Opens the working directory `cwd` below `exec`, making each part that is not there with
/// `make` (as the user's). A part that is a link is followed only to a directory below
/// `exec`.
///
/// # Errors
///
/// The path leaves `exec` ([`CwdError::Escapes`]), or a part is not a directory or cannot be
/// made, or the directory cannot be opened ([`CwdError::Unusable`]).
pub(super) fn open(
    exec: &Path,
    cwd: &str,
    make: super::files::MakeDir<'_>,
) -> Result<File, CwdError> {
    check(cwd)?;
    let unusable = |why: io::Error| CwdError::Unusable(cwd.to_owned(), why);
    let root = fs::canonicalize(exec).map_err(unusable)?;
    let mut at: PathBuf = root.clone();
    for part in Path::new(cwd).components() {
        let Component::Normal(name) = part else {
            continue;
        };
        at.push(name);
        match fs::symlink_metadata(&at) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => make(&at).map_err(unusable)?,
            Err(why) => return Err(unusable(why)),
            Ok(meta) if meta.file_type().is_symlink() => {
                let real = fs::canonicalize(&at).map_err(unusable)?;
                if !real.starts_with(&root) {
                    return Err(CwdError::Escapes(cwd.to_owned()));
                }
                at = real;
            }
            Ok(_) => {}
        }
        if !fs::metadata(&at).map_err(unusable)?.is_dir() {
            let why = io::Error::new(io::ErrorKind::NotADirectory, "not a directory");
            return Err(unusable(why));
        }
    }
    File::open(&at).map_err(unusable)
}
