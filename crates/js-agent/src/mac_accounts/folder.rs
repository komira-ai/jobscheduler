//! The per-user folders (`/private/var/folders/<xx>/<hash>`) of the slots: emptied after a
//! job, looked at before an account is made and when it is shown gone.
//!
//! macOS makes one for a user the first time a process of the uid asks for its temporary or
//! cache directory, and finds it again by the record's uid and `GeneratedUID`. System
//! Integrity Protection refuses the removal of the folder and of its top-level directories
//! (`0`, `C`, `T`, `X`), to root as well, and so does `sysadminctl -deleteUser`: they stay for
//! good, one folder for each slot, and what a job left below them is removed. Every folder two
//! levels down that the uid owns is the slot's, whatever record made it: an older account of
//! the same uid may have left another.
//!
//! Emptied is the state the proof shows and the free check requires: nothing but directories
//! at the top level of a folder, and nothing in them but the sealed items macOS's own daemons
//! make there (`tree::kept::SYSTEM_KEPT`), which root may not remove. The walk is in `tree`.

use std::io;
use std::path::{Path, PathBuf};

use super::tree::{self, Os};

/// Empties every per-user folder in `folders` that `uid` owns: each stays, with its top-level
/// directories, and everything else in it goes ([`tree::empty_folder`]). A folder that fails
/// does not stop the others.
///
/// # Errors
/// `folders` could not be searched, or a folder could not be emptied (every failure is
/// named).
pub(super) fn empty(folders: &Path, uid: u32, os: &dyn Os) -> io::Result<()> {
    let failed: Vec<String> = tree::folder_entries(folders, uid)?
        .iter()
        .filter_map(|folder| tree::empty_folder(folder, uid, os).err())
        .map(|why| why.to_string())
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(io::Error::other(failed.join("; "))),
    }
}

/// What the per-user folders `uid` owns hold that emptied ones do not: each non-directory at
/// a folder's top level, and each entry directly below its top-level directories, except the
/// sealed items macOS's own daemons make there (`tree::kept::SYSTEM_KEPT`).
///
/// # Errors
/// `folders` could not be searched, or a folder could not be looked at: what was not looked at
/// is never read as empty.
pub(super) fn left(folders: &Path, uid: u32, os: &dyn Os) -> io::Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    for folder in tree::folder_entries(folders, uid)? {
        left.extend(tree::leftovers(&folder, os)?);
    }
    Ok(left)
}

#[cfg(test)]
mod tests;
