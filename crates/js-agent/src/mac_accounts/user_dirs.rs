//! An account's own per-user temporary and cache folders: what macOS gives the account as
//! `DARWIN_USER_TEMP_DIR` and `DARWIN_USER_CACHE_DIR`, inside the slot's per-user folder
//! below `/private/var/folders`. A native job may write there (`xcrun` keeps its cache there,
//! whatever `TMPDIR` says): they belong to the one account, no other job can reach them, and
//! the retirement empties them.
//!
//! The paths are asked of the system as the account (`getconf`, which also makes the folders
//! when the slot has none yet), and each answer is checked here before a profile grants it.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", test))]
use super::Account;
#[cfg(any(target_os = "macos", test))]
use super::cycle::Ran;

/// The two names asked: the per-user temporary folder, then the per-user cache folder.
pub const NAMES: [&str; 2] = ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"];

/// The folder `printed` names (what `getconf <name>` printed for the account `uid`), as the
/// kernel resolves it: a directory below `folders` (the per-user folders' root, canonical),
/// not the root itself, that `uid` owns.
///
/// # Errors
/// Nothing was printed, the path does not resolve, or it is not a directory below `folders`
/// that `uid` owns; the text names the path.
pub fn checked(printed: &str, folders: &Path, uid: u32) -> io::Result<PathBuf> {
    let given = printed.trim();
    if given.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getconf printed no folder",
        ));
    }
    let named = |why: String| io::Error::new(io::ErrorKind::InvalidData, format!("{given}: {why}"));
    let (real, meta) = fs::canonicalize(given)
        .and_then(|real| fs::metadata(&real).map(|meta| (real, meta)))
        .map_err(|why| named(why.to_string()))?;
    if real == folders || !real.starts_with(folders) {
        return Err(named(format!("not below {}", folders.display())));
    }
    if !meta.is_dir() {
        return Err(named("not a directory".to_owned()));
    }
    if meta.uid() != uid {
        return Err(named(format!("owned by uid {}, not uid {uid}", meta.uid())));
    }
    Ok(real)
}

/// The folders of `account` below `root`: each of [`NAMES`] asked through `run` (which runs
/// the given `getconf` command as the account) and checked ([`checked`]).
///
/// # Errors
/// `run` failed, `getconf` ended with a status other than 0 (the text names the variable,
/// the account and what `getconf` printed to its error stream), or an answer is not the
/// account's own folder.
#[cfg(any(target_os = "macos", test))]
pub(super) fn folders_via(
    account: &Account,
    root: &Path,
    run: &mut dyn FnMut(&[String]) -> io::Result<Ran>,
) -> io::Result<Vec<PathBuf>> {
    NAMES
        .iter()
        .map(|name| {
            let command = ["/usr/bin/getconf".to_owned(), (*name).to_owned()];
            let ran = run(&command)?;
            if ran.code != Some(0) {
                return Err(io::Error::other(format!(
                    "getconf {name} as {}: {}",
                    account.name,
                    ran.stderr.trim()
                )));
            }
            checked(&ran.stdout, root, account.uid)
        })
        .collect()
}

#[cfg(test)]
mod tests;
