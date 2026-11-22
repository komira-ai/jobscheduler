//! What the pool does to files: removes what an account left, and looks for what is left.
//!
//! Root walks places other accounts can write while their jobs run, so no walk follows a
//! symbolic link: every directory is opened relative to its parent with `O_NOFOLLOW` and
//! checked to be the directory that was examined (device and inode), every entry is
//! examined with `AT_SYMLINK_NOFOLLOW`, and a link is unlinked, never followed. A walk
//! stays on the filesystem of the directory it starts in. The user flags that block removal
//! (`uchg`, `uappnd`) are cleared on macOS, only on entries the departing uid owns.
//!
//! Two walks share one traversal ([`walk`]): a removal of every entry a uid owns in a place
//! ([`sweep_owned`]) and a read-only look for them ([`owned_in`]). Both search the
//! directories of other owners down to [`MAX_DEPTH`], and both fail, naming the path, on a
//! directory they cannot search: what was not searched is never read as holding nothing of
//! the uid. An entry that is gone by the time a step reaches it is not an error (a system
//! daemon removing its own temporary file): it is what was wanted. Every other error is.
//!
//! A tree of the departing uid of any depth is removed: below [`PARK_DEPTH`] levels the
//! removal moves the subtree up under the directory it is emptying (a rename on one
//! filesystem, nothing followed) and removes it from there, so a job that builds a deep tree
//! cannot stop its account's deletion, and no removal holds more than a bounded number of
//! descriptors.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use super::settings::Places;

#[cfg(target_os = "linux")]
pub(super) type Dev = u64;
#[cfg(not(target_os = "linux"))]
pub(super) type Dev = i32;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How deeply a walk searches the directories of other owners for the uid's entries. A
/// walk holds one descriptor per level (a Mac's soft limit can be 256 in all), so this and
/// [`PARK_DEPTH`] together stay well under that.
const MAX_DEPTH: usize = 64;

/// How deep a removal recurses before it moves what is below up to the top.
const PARK_DEPTH: usize = 64;

/// How many names [`Park::take`] tries before it gives up.
const PARK_NAMES: usize = 4096;

/// `result`, with "no such file or directory" as `None`: a place that does not exist holds
/// nothing.
fn present<T>(result: Result<T, Errno>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Errno::NOENT) => Ok(None),
        Err(why) => Err(why.into()),
    }
}

/// `result`, with "no such file or directory" taken as success: an entry that is gone
/// between a walk's listing of it and its removal is what the walk wanted.
fn vanished(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Failures that already name the paths they are about: what [`joined`] makes of the failures
/// of a walk, so that a directory that holds a failure is not named in front of it again at
/// every level above.
#[derive(Debug)]
struct Named(String);

impl std::fmt::Display for Named {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Named {}

/// Whether `why` is a [`Named`] failure.
fn is_named(why: &io::Error) -> bool {
    why.get_ref().is_some_and(|inner| inner.is::<Named>())
}

/// `why` with `path` put in front of it, unless it names its paths already.
fn locate(path: &Path, why: &io::Error) -> String {
    match is_named(why) {
        true => why.to_string(),
        false => format!("{}: {why}", path.display()),
    }
}

/// Records `result`'s error, if any, against `path`.
fn note(failures: &mut Vec<String>, path: &Path, result: io::Result<()>) {
    if let Err(why) = result {
        failures.push(locate(path, &why));
    }
}

/// `Ok` for no failures, otherwise one error that names them all.
fn joined(failures: &[String]) -> io::Result<()> {
    match failures.is_empty() {
        true => Ok(()),
        false => Err(io::Error::other(Named(failures.join("; ")))),
    }
}

/// `why` as an error with `path` put in front of it, unless it names its paths already.
fn located(path: &Path, why: io::Error) -> io::Error {
    match is_named(&why) {
        true => why,
        false => io::Error::new(why.kind(), locate(path, &why)),
    }
}

/// `result`'s error with `path` put in front of it, unless it names its paths already.
fn named<T>(path: &Path, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|why| located(path, why))
}

#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn is_dir(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode as _) == FileType::Directory
}

/// The directory `path`, not followed if it is a link; `None` if it does not exist.
fn open_dir(path: &Path) -> io::Result<Option<OwnedFd>> {
    present(rustix::fs::openat(CWD, path, DIRECTORY, Mode::empty()))
}

/// The directory that holds `path` and the entry's name in it.
fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path to an entry",
        )),
    }
}

/// The names in `dir`, read whole before any is removed.
fn list(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(names)
}

/// `name` in `dir`, examined without following a link; `None` if it is gone.
fn examine(dir: &OwnedFd, name: &OsStr) -> io::Result<Option<Stat>> {
    present(rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW))
}

/// Opens directory `name` in `dir` without following a link, and checks it is the
/// directory `stat` examined: one swapped in since is refused.
fn open_child(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<OwnedFd> {
    let child = rustix::fs::openat(dir, name, DIRECTORY, Mode::empty())?;
    let opened = rustix::fs::fstat(&child)?;
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
        return Err(io::Error::other("replaced while being examined"));
    }
    Ok(child)
}

/// Removes the empty directory `name` of `dir`.
fn rmdir_at(dir: &OwnedFd, name: &OsStr) -> Result<(), Errno> {
    rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)
}

/// Clears the user flags that block removing the entry `name` of `dir`, examined as `stat`.
#[cfg(target_os = "macos")]
fn unlock(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> io::Result<()> {
    super::macos::clear_user_flags(dir, name, stat.st_flags)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // the macOS variant can fail
fn unlock(_dir: &OwnedFd, _name: &OsStr, _stat: &Stat) -> io::Result<()> {
    Ok(())
}

/// `renameat` that does not replace what is at the new name (`EEXIST` instead).
#[cfg(target_os = "linux")]
fn rename_excl(from: &OwnedFd, old: &OsStr, to: &OwnedFd, new: &OsStr) -> Result<(), Errno> {
    rustix::fs::renameat_with(from, old, to, new, rustix::fs::RenameFlags::NOREPLACE)
}

#[cfg(target_os = "macos")]
fn rename_excl(from: &OwnedFd, old: &OsStr, to: &OwnedFd, new: &OsStr) -> Result<(), Errno> {
    super::macos::rename_excl(from, old, to, new)
}

/// Where a removal puts a subtree deeper than [`PARK_DEPTH`]: `dir`, the directory being
/// emptied (same filesystem, so a rename, and the subtree is only moved, never copied or
/// followed). [`Park::drain`] removes what was moved up; it is [`PARK_DEPTH`] levels
/// shallower, so a tree of any depth is gone after as many passes as it has multiples of
/// that. A removal that fails part-way leaves everything of the uid under `dir`, where the
/// next removal finds it again.
struct Park<'a> {
    dir: &'a OwnedFd,
    names: std::cell::RefCell<Vec<OsString>>,
    next: std::cell::Cell<u32>,
}

impl<'a> Park<'a> {
    fn new(dir: &'a OwnedFd) -> Self {
        Self {
            dir,
            names: std::cell::RefCell::default(),
            next: std::cell::Cell::new(0),
        }
    }

    /// Moves `name` of `from`, judged as `judged`, up to a name of its own in the park.
    /// It is looked at again first: an entry replaced since it was judged is not moved
    /// (an error). The move is exclusive, so a name made ahead is skipped, never replaced.
    fn take(&self, from: &OwnedFd, name: &OsStr, judged: &Stat) -> io::Result<()> {
        let same = examine(from, name)?
            .is_some_and(|now| (now.st_dev, now.st_ino) == (judged.st_dev, judged.st_ino));
        if !same {
            return Err(io::Error::other("replaced while being examined"));
        }
        for _ in 0..PARK_NAMES {
            let n = self.next.get();
            self.next.set(n + 1);
            let to = OsString::from(format!(".kbf-deep-{}-{n}", std::process::id()));
            match rename_excl(from, name, self.dir, to.as_os_str()) {
                Ok(()) => {
                    self.names.borrow_mut().push(to);
                    return Ok(());
                }
                // Taken: the next name.
                Err(Errno::EXIST) => {}
                Err(why) => return Err(why.into()),
            }
        }
        Err(io::Error::other(
            "no free name to move a deep tree up under",
        ))
    }

    /// Removes what was moved up, which may move more up, until nothing is left.
    fn drain(&self, uid: u32, dev: Dev) -> io::Result<()> {
        loop {
            let Some(name) = self.names.borrow_mut().pop() else {
                return Ok(());
            };
            let Some(stat) = examine(self.dir, &name)? else {
                continue;
            };
            let here = Path::new(&name);
            remove_entry(self.dir, &name, here, &stat, (uid, dev, 1), self)?;
        }
    }
}

/// Removes `name` in `dir`, examined as `stat`: unlinked, or a directory emptied
/// (everything in it, whoever owns it) and removed. Only entries `uid` owns are unlocked
/// first. An entry that is gone by the time a step reaches it, `name` or anything below it,
/// is removed already.
fn remove_entry(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    stat: &Stat,
    ctx: (u32, Dev, usize),
    park: &Park,
) -> io::Result<()> {
    vanished(remove_present(dir, name, here, stat, ctx, park))
}

/// [`remove_entry`] with every step's error as it is, "no such file or directory" included.
fn remove_present(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    stat: &Stat,
    (uid, dev, depth): (u32, Dev, usize),
    park: &Park,
) -> io::Result<()> {
    if stat.st_dev != dev {
        return Err(io::Error::other(
            "another filesystem is mounted here; not crossed",
        ));
    }
    if stat.st_uid == uid {
        unlock(dir, name, stat)?;
    }
    if !is_dir(stat) {
        return Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?);
    }
    if depth > PARK_DEPTH {
        return park.take(dir, name, stat);
    }
    let child = open_child(dir, name, stat)?;
    empty(&child, here, (uid, dev, depth + 1), park)?;
    Ok(rmdir_at(dir, name)?)
}

/// Removes every entry of `dir` (at `path`), whoever owns it; the entries are at depth
/// `depth`. The removal goes on past a refusal: everything that can be removed is, and the
/// refusals are judged together.
///
/// # Errors
/// `dir` cannot be listed, or an entry could not be removed (each is named).
fn empty(
    dir: &OwnedFd,
    path: &Path,
    (uid, dev, depth): (u32, Dev, usize),
    park: &Park,
) -> io::Result<()> {
    let mut failed = Vec::new();
    for entry in list(dir)? {
        let inner = path.join(&entry);
        let result = examine(dir, &entry).and_then(|found| {
            found.map_or(Ok(()), |stat| {
                remove_entry(dir, &entry, &inner, &stat, (uid, dev, depth), park)
            })
        });
        note(&mut failed, &inner, result);
    }
    joined(&failed)
}

/// Removes the entry `name` of `dir`, examined as `stat`, and all it holds, parking a deep
/// subtree inside the entry itself.
fn remove_whole(dir: &OwnedFd, name: &OsStr, path: &Path, stat: &Stat, uid: u32) -> io::Result<()> {
    let dev = rustix::fs::fstat(dir)?.st_dev;
    if !is_dir(stat) || stat.st_dev != dev {
        return remove_entry(dir, name, path, stat, (uid, dev, 1), &Park::new(dir));
    }
    // The entry is a directory on the parent's filesystem: what it holds is removed with
    // the entry itself as the park, then the entry.
    let entry = open_child(dir, name, stat)?;
    if stat.st_uid == uid {
        unlock(dir, name, stat)?;
    }
    let park = Park::new(&entry);
    let mut failed = Vec::new();
    failed.extend(
        empty(&entry, path, (uid, dev, 2), &park)
            .err()
            .map(|why| why.to_string()),
    );
    note(&mut failed, path, park.drain(uid, dev));
    joined(&failed)?;
    Ok(rmdir_at(dir, name)?)
}

/// Removes the entry at `path`, whatever it is and whoever owns it, with all it holds;
/// returns whether there was one. Never through a link. The directory above it is opened
/// without following a link and must exist: no directory above it is no entry.
///
/// # Errors
/// The entry, or something in it, could not be removed (all the failures are named), or
/// `path` is not a path to an entry.
pub(super) fn remove_named(path: &Path, uid: u32) -> io::Result<bool> {
    let (parent, name) = split(path)?;
    let Some(dir) = named(parent, open_dir(parent))? else {
        return Ok(false);
    };
    let Some(stat) = named(path, examine(&dir, name))? else {
        return Ok(false);
    };
    named(path, remove_whole(&dir, name, path, &stat, uid))?;
    Ok(true)
}

/// Whether anything is at `path` (a link counts, and is not followed); no directory above
/// it is no entry.
///
/// # Errors
/// `path` is not a path to an entry, or the directory above it or the entry could not be
/// looked at.
pub(super) fn exists(path: &Path) -> io::Result<bool> {
    let (parent, name) = split(path)?;
    let Some(dir) = named(parent, open_dir(parent))? else {
        return Ok(false);
    };
    Ok(named(path, examine(&dir, name))?.is_some())
}

/// An entry of a walk that `uid` owns: where it is, and how it was examined.
struct Found<'a> {
    dir: &'a OwnedFd,
    name: &'a OsStr,
    here: &'a Path,
    stat: &'a Stat,
}

impl Found<'_> {
    /// Removes the entry whole, as [`remove_entry`] does.
    fn remove(&self, ctx: (u32, Dev, usize), park: &Park) -> io::Result<()> {
        remove_entry(self.dir, self.name, self.here, self.stat, ctx, park)
    }
}

/// Every entry of `dir` (at `path`) that `uid` owns is handed to `found`; the directories of
/// other owners on `dev` are searched below, to [`MAX_DEPTH`]. Errors below `dir` are
/// recorded per entry in `failed` and the walk goes on, so one stuck entry does not hide the
/// others.
///
/// # Errors
/// `dir` cannot be listed.
fn walk(
    dir: &OwnedFd,
    path: &Path,
    (uid, dev, depth): (u32, Dev, usize),
    found: &mut dyn FnMut(&Found<'_>) -> io::Result<()>,
    failed: &mut Vec<String>,
) -> io::Result<()> {
    for name in list(dir)? {
        let here = path.join(&name);
        let result = visit(dir, &name, &here, (uid, dev, depth), found, failed);
        note(failed, &here, result);
    }
    Ok(())
}

/// One entry of [`walk`]'s directory.
fn visit(
    dir: &OwnedFd,
    name: &OsStr,
    here: &Path,
    (uid, dev, depth): (u32, Dev, usize),
    found: &mut dyn FnMut(&Found<'_>) -> io::Result<()>,
    failed: &mut Vec<String>,
) -> io::Result<()> {
    let Some(stat) = examine(dir, name)? else {
        return Ok(());
    };
    // Another filesystem mounted here is never entered.
    if stat.st_dev != dev {
        return Ok(());
    }
    if stat.st_uid == uid {
        return found(&Found {
            dir,
            name,
            here,
            stat: &stat,
        });
    }
    if !is_dir(&stat) || depth >= MAX_DEPTH {
        return Ok(());
    }
    // Another owner's directory is searched for the uid's entries. One gone since it was
    // examined holds nothing; one that cannot be searched fails the walk.
    vanished(
        open_child(dir, name, &stat)
            .and_then(|child| walk(&child, here, (uid, dev, depth + 1), found, failed)),
    )
}

/// Every entry below `place` that `uid` owns, at any depth below other owners'
/// directories (the entry itself, not what is in it); nothing for a `place` that does not
/// exist.
///
/// # Errors
/// A directory could not be opened, listed or searched, or an entry could not be examined
/// (every failure is named).
pub(super) fn owned_in(place: &Path, uid: u32) -> io::Result<Vec<PathBuf>> {
    let Some(dir) = named(place, open_dir(place))? else {
        return Ok(Vec::new());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    let mut owned = Vec::new();
    let mut failed = Vec::new();
    let mut collect = |entry: &Found<'_>| -> io::Result<()> {
        owned.push(entry.here.to_path_buf());
        Ok(())
    };
    let walked = walk(&dir, place, (uid, dev, 0), &mut collect, &mut failed);
    named(place, walked)?;
    joined(&failed)?;
    Ok(owned)
}

/// Removes every entry below `place` that `uid` owns, each whole, as [`owned_in`] finds
/// them; returns how many. A `place` that does not exist holds nothing to remove.
///
/// # Errors
/// What [`owned_in`] fails on, or an entry that could not be removed (every failure is
/// named).
pub(super) fn sweep_owned(place: &Path, uid: u32) -> io::Result<usize> {
    let Some(dir) = named(place, open_dir(place))? else {
        return Ok(0);
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    let park = Park::new(&dir);
    let mut removed = 0;
    let mut failed = Vec::new();
    let mut remove = |entry: &Found<'_>| -> io::Result<()> {
        entry.remove((uid, dev, 1), &park)?;
        removed += 1;
        Ok(())
    };
    let walked = walk(&dir, place, (uid, dev, 0), &mut remove, &mut failed);
    note(&mut failed, place, park.drain(uid, dev));
    named(place, walked)?;
    joined(&failed)?;
    Ok(removed)
}

/// `template` with the account's name and uid put in.
fn filled(template: &str, name: &str, uid: u32) -> PathBuf {
    PathBuf::from(
        template
            .replace("{uid}", &uid.to_string())
            .replace("{name}", name),
    )
}

/// [`filled`] for the account `name`, `None` for a template that names the account when it
/// is not known: a uid alone does not name it.
fn looked_at(template: &str, name: Option<&str>, uid: u32) -> Option<PathBuf> {
    match name {
        Some(name) => Some(filled(template, name, uid)),
        None if template.contains("{name}") => None,
        None => Some(filled(template, "", uid)),
    }
}

/// Removes what `places` holds of the account `name` with `uid`: each named entry whole,
/// and every entry `uid` owns in each owned directory. Returns how many were removed.
///
/// # Errors
/// Something could not be removed; every place that failed is named, and the others are
/// done.
pub(super) fn sweep(places: &Places, name: &str, uid: u32) -> io::Result<usize> {
    let mut removed = 0;
    let mut failed = Vec::new();
    for template in &places.named {
        match remove_named(&filled(template, name, uid), uid) {
            Ok(was) => removed += usize::from(was),
            Err(why) => failed.push(why.to_string()),
        }
    }
    for place in &places.owned {
        match sweep_owned(place, uid) {
            Ok(count) => removed += count,
            Err(why) => failed.push(why.to_string()),
        }
    }
    joined(&failed)?;
    Ok(removed)
}

/// Everything `places` holds that is named for the account `name` (when it is known) or
/// owned by `uid`, read-only. A template that names the account is not looked at when
/// `name` is `None`: a uid alone does not name it.
///
/// # Errors
/// A place could not be read (every failure is named).
pub(super) fn left_in(places: &Places, name: Option<&str>, uid: u32) -> io::Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    let mut failed = Vec::new();
    for template in &places.named {
        let Some(path) = looked_at(template, name, uid) else {
            continue;
        };
        match exists(&path) {
            Ok(true) => left.push(path),
            Ok(false) => {}
            Err(why) => failed.push(why.to_string()),
        }
    }
    for place in &places.owned {
        match owned_in(place, uid) {
            Ok(found) => left.extend(found),
            Err(why) => failed.push(why.to_string()),
        }
    }
    joined(&failed)?;
    Ok(left)
}

/// The directories of the per-user folders `folders` (`<folders>/<xx>/<hash>`) that `uid`
/// owns; none when `folders` does not exist. Nothing is followed: a link is not an entry.
///
/// # Errors
/// `folders` or one of its `<xx>` directories could not be read.
pub(super) fn folder_entries(folders: &Path, uid: u32) -> io::Result<Vec<PathBuf>> {
    let Some(root) = named(folders, open_dir(folders))? else {
        return Ok(Vec::new());
    };
    named(folders, entries_below(&root, folders, uid))
}

/// [`folder_entries`] of the opened `root` (at `folders`).
fn entries_below(root: &OwnedFd, folders: &Path, uid: u32) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for xx in list(root)? {
        // Not an entry: a file, a link, or one gone since it was listed.
        let Some(stat) = examine(root, &xx)?.filter(is_dir) else {
            continue;
        };
        let shard = open_child(root, &xx, &stat)?;
        for hash in list(&shard)? {
            // Without short-circuits: both are read.
            let owned =
                examine(&shard, &hash)?.filter(|inner| is_dir(inner) & (inner.st_uid == uid));
            if owned.is_some() {
                found.push(folders.join(&xx).join(&hash));
            }
        }
    }
    Ok(found)
}

/// Removes the directory at `path`, an entry of the per-user folders, and everything in it,
/// whoever owns what is inside. Nothing is removed unless `path` is a directory that `uid`
/// owns on its parent's filesystem: one of another owner, or a file or a link, is an error
/// and stays as it is. A `path` that does not exist holds nothing. The system placed the
/// entry and may refuse its removal (`sunlnk`): the refusal is an error naming it.
///
/// # Errors
/// The entry is not the uid's directory, or it or something in it could not be removed.
pub(super) fn remove_folder_entry(path: &Path, uid: u32) -> io::Result<()> {
    let (parent, name) = split(path)?;
    let Some(dir) = named(parent, open_dir(parent))? else {
        return Ok(());
    };
    let Some(stat) = named(path, examine(&dir, name))? else {
        return Ok(());
    };
    let dev = rustix::fs::fstat(&dir)?.st_dev;
    // Without short-circuits: both are read.
    if !(is_dir(&stat) & (stat.st_dev == dev)) {
        return Err(io::Error::other(format!(
            "{}: not a directory on its parent's filesystem; not removed",
            path.display()
        )));
    }
    if stat.st_uid != uid {
        return Err(io::Error::other(format!(
            "{}: owned by uid {}, not uid {uid}; not removed",
            path.display(),
            stat.st_uid
        )));
    }
    named(path, remove_whole(&dir, name, path, &stat, uid))
}

#[cfg(test)]
mod tests;
