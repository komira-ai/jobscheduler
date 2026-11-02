//! [`remove_owned`]: one directory of the departing uid removed whole, its owner checked
//! first (the uid's entry under the per-user folders, which `user-delete` names from the
//! uid's `DARWIN_USER_DIR`).

use std::io;
use std::path::Path;

use rustix::fs::AtFlags;

use super::{Park, Swept, is_dir, note, open_base, present, remove_entry};

/// Removes the directory at `path` and everything in it, whoever owns what is inside, as
/// the sweep removes an entry: never through a link, never past the filesystem of its
/// parent, the user flags cleared only on what `uid` owns. Nothing is removed unless
/// `path` is a directory that `uid` owns: one of another owner, or a file or a link, is
/// an error and stays as it is. A `path` that does not exist holds nothing.
#[must_use]
pub(crate) fn remove_owned(path: &Path, uid: u32) -> Swept {
    let mut swept = Swept::default();
    let result = remove_dir_of(path, uid, &mut swept);
    note(&mut swept, path, result);
    swept
}

/// [`remove_owned`], with its one error.
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
    if !is_dir(&stat) {
        return Err(io::Error::other("not a directory; not removed"));
    }
    if stat.st_uid != uid {
        return Err(io::Error::other(format!(
            "owned by uid {}, not uid {uid}; not removed",
            stat.st_uid
        )));
    }
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    let park = Park::new(&dir);
    remove_entry(&dir, name, path, &stat, (uid, dev, 1), &park)?;
    park.drain(uid, dev)?;
    swept.removed += 1;
    Ok(())
}

#[cfg(test)]
mod tests;
