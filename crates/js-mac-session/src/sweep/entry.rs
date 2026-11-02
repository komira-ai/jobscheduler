//! The uid's entries under the per-user folders (`/private/var/folders/<xx>/<hash>`):
//! [`remove_owned`] removes one whole, its owner checked first, and [`owned_entries`]
//! finds those the uid owns when there is no `DARWIN_USER_DIR` left to name one.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::AtFlags;

use super::{
    Park, Swept, examine, is_dir, listed, note, open_base, open_child, present, remove_entry,
    rmdir_at, unlock,
};

/// Removes the directory at `path` and everything in it, whoever owns what is inside, as
/// the sweep removes an entry: never through a link, never past the filesystem of its
/// parent, the user flags cleared only on what `uid` owns. Nothing is removed unless
/// `path` is a directory that `uid` owns: one of another owner, or a file or a link, is
/// an error and stays as it is. A `path` that does not exist holds nothing.
///
/// A subtree too deep to remove in place is moved up inside `path` itself, never beside
/// it, so a removal that fails part-way (or is cut short) leaves everything of the uid
/// under `path`, where the next removal of the same entry finds it again.
#[must_use]
pub(crate) fn remove_owned(path: &Path, uid: u32) -> Swept {
    let mut swept = Swept::default();
    let result = remove_dir_of(path, uid, &mut swept);
    note(&mut swept, path, result);
    swept
}

/// [`remove_owned`]: the entry's own error, the errors of what is in it noted in `swept`.
fn remove_dir_of(path: &Path, uid: u32, swept: &mut Swept) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path to an entry",
        ));
    };
    let Some(dir) = open_base(parent)? else {
        return Ok(());
    };
    let Some(stat) = present(rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW))? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    // Without short-circuits: both are read.
    if !(is_dir(&stat) & (stat.st_dev == dev)) {
        return Err(io::Error::other(
            "not a directory on its parent's filesystem; not removed",
        ));
    }
    if stat.st_uid != uid {
        return Err(io::Error::other(format!(
            "owned by uid {}, not uid {uid}; not removed",
            stat.st_uid
        )));
    }
    let entry = open_child(&dir, name, &stat)?;
    unlock(&dir, name, &stat)?;
    let before = swept.errors.len();
    let park = Park::new(&entry);
    for inner in listed(&entry)? {
        let here = path.join(&inner);
        let result = examine(&entry, &inner).and_then(|found| {
            found.map_or(Ok(()), |st| {
                remove_entry(&entry, &inner, &here, &st, (uid, dev, 2), &park)
            })
        });
        note(swept, &here, result);
    }
    note(swept, path, park.drain(uid, dev));
    if swept.errors.len() > before {
        // Kept, with all that is left of it inside, for the next removal.
        return Ok(());
    }
    rmdir_at(&dir, name)?;
    swept.removed += 1;
    Ok(())
}

/// The entries of the per-user folders `folders` (`<folders>/<xx>/<hash>`) that are
/// directories `uid` owns, on the filesystem of `folders`; none when `folders` does not
/// exist. Nothing is followed: a link is not an entry.
///
/// # Errors
/// `folders` or one of its `<xx>` directories could not be read.
pub(crate) fn owned_entries(folders: &Path, uid: u32) -> io::Result<Vec<PathBuf>> {
    let Some(root) = open_base(folders)? else {
        return Ok(Vec::new());
    };
    let dev = rustix::fs::fstat(&root)?.st_dev;
    let mut found = Vec::new();
    for xx in listed(&root)? {
        let Some(stat) = examine(&root, &xx)? else {
            continue;
        };
        // Without short-circuits: both are read.
        if !(is_dir(&stat) & (stat.st_dev == dev)) {
            continue;
        }
        let shard = open_child(&root, &xx, &stat)?;
        for hash in listed(&shard)? {
            let Some(inner) = examine(&shard, &hash)? else {
                continue;
            };
            if is_dir(&inner) & (inner.st_uid == uid) & (inner.st_dev == dev) {
                found.push(folders.join(&xx).join(&hash));
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
