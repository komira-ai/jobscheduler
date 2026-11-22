//! The job's files: the inputs copied in, the outputs copied out, and the check that the user
//! has no new or changed file outside the job folder.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, lchown, symlink};
use std::path::{Path, PathBuf};

use super::{copied_mode, set_mode};
use crate::mac_job::report::Outputs;

/// The most entries a scan of the user's files records; past it the scan says so.
const SCAN_MAX: usize = 500_000;

/// The most changed files a report lists.
const LISTED_MAX: usize = 500;

/// Where a job's user could have files outside the job folder: the shared temporary folders,
/// the user's home, and the user's own folders under `/var/folders`.
pub(super) fn scan_roots(home: &str, uid: u32) -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/tmp"),
        PathBuf::from(home),
    ];
    // `/private/var/folders/<xx>/<id>`: the user's temp (`T`), cache (`C`) and others.
    if let Ok(top) = fs::read_dir("/private/var/folders") {
        for xx in top.flatten() {
            let Ok(ids) = fs::read_dir(xx.path()) else {
                continue;
            };
            for id in ids.flatten() {
                if id.metadata().is_ok_and(|m| m.uid() == uid) {
                    roots.push(id.path());
                }
            }
        }
    }
    roots
}

/// The user's files under some roots, each with its change time.
pub(super) struct Snapshot {
    /// Path to (seconds, nanoseconds) of its last status change.
    entries: BTreeMap<PathBuf, (i64, i64)>,
}

impl Snapshot {
    /// Records `uid`'s files under `roots`, not following links.
    pub(super) fn take(roots: &[PathBuf], uid: u32) -> Self {
        let mut entries = BTreeMap::new();
        for root in roots {
            walk(root, uid, &mut entries, 0);
        }
        Self { entries }
    }

    /// `uid`'s files under `roots` that are new or changed since this snapshot.
    pub(super) fn changed(&self, roots: &[PathBuf], uid: u32) -> Vec<String> {
        let now = Self::take(roots, uid);
        let mut list: Vec<String> = now
            .entries
            .iter()
            .filter(|(path, ctime)| self.entries.get(*path) != Some(ctime))
            .map(|(path, _)| path.display().to_string())
            .collect();
        if list.len() > LISTED_MAX {
            let more = list.len() - LISTED_MAX;
            list.truncate(LISTED_MAX);
            list.push(format!("... and {more} more"));
        }
        if now.entries.len() >= SCAN_MAX {
            list.push(format!("(the scan stopped at {SCAN_MAX} files)"));
        }
        list
    }
}

/// Records `uid`'s entries at and under `path`.
fn walk(path: &Path, uid: u32, into: &mut BTreeMap<PathBuf, (i64, i64)>, depth: usize) {
    if into.len() >= SCAN_MAX || depth > 64 {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.uid() == uid {
        into.insert(path.to_owned(), (meta.ctime(), meta.ctime_nsec()));
    }
    if meta.is_dir()
        && let Ok(children) = fs::read_dir(path)
    {
        for child in children.flatten() {
            walk(&child.path(), uid, into, depth + 1);
        }
    }
}

/// Removes the job folder: first clears every file flag under it (a job may set the
/// user-immutable flag, which stops even root's unlink), then removes the tree.
pub(super) fn remove_folder(folder: &Path) -> io::Result<()> {
    clear_tree_flags(folder);
    fs::remove_dir_all(folder)
}

/// Clears the flags of `path` and of everything under it, not following links.
fn clear_tree_flags(path: &Path) {
    let _ = super::sys::clear_flags(path);
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && let Ok(children) = fs::read_dir(path)
    {
        for child in children.flatten() {
            clear_tree_flags(&child.path());
        }
    }
}

/// Copies the tree at `from` into the existing folder `to`, links as links, and gives every
/// entry to `owner` when one is named. Set-id bits are dropped; other file types are skipped.
pub(super) fn copy_tree(from: &Path, to: &Path, owner: Option<(u32, u32)>) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_symlink() {
            symlink(fs::read_link(entry.path())?, &dest)?;
        } else if kind.is_dir() {
            fs::create_dir(&dest)?;
            set_mode(&dest, 0o755)?;
            copy_tree(&entry.path(), &dest, owner)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &dest)?;
            set_mode(&dest, copied_mode(entry.metadata()?.mode()))?;
        } else {
            continue;
        }
        if let Some((uid, gid)) = owner {
            lchown(&dest, Some(uid), Some(gid))?;
        }
    }
    Ok(())
}

/// Copies each declared output from `work` to `to`, refusing one reached through a link.
pub(super) fn collect(work: &Path, outputs: &[String], to: &Path) -> Outputs {
    let mut result = Outputs::default();
    for output in outputs {
        match collect_one(work, output, to) {
            Ok(()) => result.copied.push(output.clone()),
            Err(why) => result.missing.push(format!("{output}: {why}")),
        }
    }
    result
}

/// Copies one output.
fn collect_one(work: &Path, output: &str, to: &Path) -> io::Result<()> {
    let mut at = work.to_owned();
    for part in output.split('/') {
        at.push(part);
        if fs::symlink_metadata(&at)?.file_type().is_symlink() {
            return Err(io::Error::other(format!("{} is a link", at.display())));
        }
    }
    let dest = to.join(output);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(&at)?.is_dir() {
        fs::create_dir_all(&dest)?;
        copy_tree(&at, &dest, None)
    } else {
        fs::copy(&at, &dest).map(|_| ())
    }
}
