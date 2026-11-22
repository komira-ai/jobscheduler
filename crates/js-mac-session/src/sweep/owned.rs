//! A read-only look at what a uid left in the places a sweep removes it from: the proof
//! that the sweep (and the deletion of the account) left nothing, and the check that a uid
//! is free of what an earlier account of it left.
//!
//! [`owned_left`] reads the same places [`super::sweep`] does, the same way: a named entry
//! is a path (with the user's name and uid put in), an owned place is walked for entries
//! the uid owns, never through a link, never past the filesystem of the place, searching
//! the directories of other owners down to [`MAX_DEPTH`]. What the sweep takes as no failure
//! the look takes as none: a top-level entry the system refuses an examination of (an Apple
//! data vault) is skipped, an entry gone since the listing is gone, and a directory of
//! another owner that cannot be searched is logged and passed over. Nothing is changed.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::AtFlags;

use super::{
    MAX_DEPTH, StDev, SweepPlan, examine, is_dir, listed, open_base, open_child, present,
    skip_protected_top, vanished,
};

/// Every entry of `plan`'s places that `uid` owns or that is named for it: a path of
/// [`SweepPlan::named`] (with `{uid}`, and `{user}` when `user` is known, put in) that
/// exists, whoever owns it, and each entry of the places of [`SweepPlan::owned`] that `uid`
/// owns, at any depth (the entry itself, not what is in it). A template that names `{user}`
/// is not looked at when `user` is `None`: a uid alone does not name it.
///
/// # Errors
/// A place could not be read: its directory could not be opened or listed, or an entry
/// could not be examined (other than the cases the module documentation names).
pub(crate) fn owned_left(
    plan: &SweepPlan,
    user: Option<&str>,
    uid: u32,
) -> io::Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    for template in &plan.named {
        let Some(path) = named_path(template, user, uid) else {
            continue;
        };
        if exists(&path)? {
            left.push(path);
        }
    }
    for base in &plan.owned {
        let Some(dir) = open_base(base)? else {
            continue;
        };
        let dev = rustix::fs::fstat(&dir)?.st_dev;
        look_in(&dir, base, (uid, dev), 0, &mut left)?;
    }
    Ok(left)
}

/// `template` with the user's name and uid put in; `None` for one that names a user when
/// there is none.
fn named_path(template: &str, user: Option<&str>, uid: u32) -> Option<PathBuf> {
    let template = template.replace("{uid}", &uid.to_string());
    match user {
        Some(user) => Some(PathBuf::from(template.replace("{user}", user))),
        None if template.contains("{user}") => None,
        None => Some(PathBuf::from(template)),
    }
}

/// Whether anything is at `path` (a link counts, and is not followed); no directory above
/// it is no entry.
fn exists(path: &Path) -> io::Result<bool> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path to an entry",
        ));
    };
    let Some(dir) = open_base(parent)? else {
        return Ok(false);
    };
    Ok(present(rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW))?.is_some())
}

/// The entries of `dir` (at `path`) that `uid` owns, and below the directories of other
/// owners on `dev`, collected in `left`.
pub(super) fn look_in(
    dir: &OwnedFd,
    path: &Path,
    (uid, dev): (u32, StDev),
    depth: usize,
    left: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for name in listed(dir)? {
        let here = path.join(&name);
        let Some(stat) = skip_protected_top(examine(dir, &name), depth, &here)? else {
            continue;
        };
        // Another filesystem mounted here is never entered.
        if stat.st_dev != dev {
            continue;
        }
        if stat.st_uid == uid {
            left.push(here);
        } else if is_dir(&stat) && depth < MAX_DEPTH {
            let searched = vanished(
                open_child(dir, &name, &stat)
                    .and_then(|child| look_in(&child, &here, (uid, dev), depth + 1, left)),
            );
            if let Err(why) = searched {
                tracing::debug!(folder = %here.display(), "not searched: {why}");
            }
        }
    }
    Ok(())
}
