//! The kept copy: the files a config package places, as they were on the host before the update,
//! so that `rollback` can put them back before it restarts the unit.
//!
//! The installer replaces a file whatever placed it, a person or an earlier package, and on a
//! first install there is no earlier version for it to put back. So `prepare`, which runs before
//! the installer places anything, keeps every file the staged package places (each file and link
//! below `<staged release>/tree`, at the same path below the host's root) and the params'
//! `flags_file`. Of each path it keeps what is there, not following a link at the path itself:
//!
//! - a regular file: its bytes, its permission bits (`mode & 0o7777`), its owner and its group;
//! - a symbolic link: its target, its owner and its group;
//! - nothing: that nothing was there, which `rollback` reports and leaves to the installer;
//! - anything else (a directory, a socket) is refused: the step ends `keep_failed`.
//!
//! The copy lives in the hook's state directory (root's, 0700), as `kept/`: `kept/index.json`
//! says what was at each path, and a file's bytes are `kept/<n>`, the `n`th entry's. It is
//! written under the name `kept.making` and renamed to `kept` when whole, so `kept` is a whole
//! copy or is not there; a `kept.making` that is found is what a killed step left, and is
//! removed. It is removed the same way: renamed to `kept.making`, then removed.
//!
//! The copy is reached through the descriptor the step's [`Held`](super::Held) opened and
//! checked the state directory as ([`StateDir`]), never by the directory's path: it is made
//! (`mkdirat`), its files written and read (`openat`, a link not followed), renamed
//! (`renameat`) and removed (`unlinkat`, walking the tree without following a link) relative to
//! that descriptor, and the directory synced through it. The steps run as root, so a state
//! directory moved and replaced by a link after the check redirects none of it. The paths in an
//! error are only the names the copy is reported by.
//!
//! A file is put back by writing a file beside it (`.<name>.js-agent-config-restore`), giving it
//! its owner, group and mode, syncing it and renaming it over the path, then syncing the
//! directory, so the path holds the file that was there or the kept one and never a part of
//! either; a link is made beside it and renamed the same way. A path that already holds what was
//! kept is not written.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, lchown};
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fsync, mkdirat, openat, renameat, statat, unlinkat,
};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

/// The kept copy's directory in the state directory.
const KEPT: &str = "kept";

/// The name the copy is written, and removed, under.
const MAKING: &str = "kept.making";

/// What was at each path.
const INDEX: &str = "index.json";

/// The format of the index; see [`Index`].
const INDEX_SCHEMA: u32 = 1;

/// The package's files in its staged release directory.
const TREE: &str = "tree";

/// The end of the name a file or link is written under before it is renamed over its path.
const RESTORE_SUFFIX: &str = ".js-agent-config-restore";

/// Why the copy could not be kept, read, removed or put back.
#[derive(Debug, thiserror::Error)]
pub(super) enum KeptError {
    /// A file operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// A path the hook cannot keep or put back.
    #[error("{}: {what}", path.display())]
    Refused {
        /// The path.
        path: PathBuf,
        /// Why.
        what: &'static str,
    },
}

/// An `Io` error at `path`, for `map_err`.
fn at(path: &Path) -> impl FnOnce(io::Error) -> KeptError + '_ {
    |source| KeptError::Io {
        path: path.to_owned(),
        source,
    }
}

/// An `Io` error at `path` from the system's answer, for `map_err`.
fn errno_at(path: &Path) -> impl FnOnce(Errno) -> KeptError + '_ {
    |errno: Errno| at(path)(errno.into())
}

/// How a directory of the copy is opened: a directory, a link not followed, not inherited by a
/// child.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How a file of the copy is read: a link not followed, not inherited by a child.
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How a file of the copy is written: made new (never one that is there, nor through a link),
/// not inherited by a child.
const NEW_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The state directory the copy is in: the descriptor the step's lock holds, checked when it
/// was opened, and the path it is reported by.
#[derive(Clone, Copy, Debug)]
pub(super) struct StateDir<'a> {
    /// The directory, as it was opened and checked ([`Held::dir_fd`](super::Held)).
    pub fd: BorrowedFd<'a>,
    /// Its name in what the step tells.
    pub path: &'a Path,
}

/// A file of the copy: the file `name` of the copy's directory `dir`, reported as `path`.
struct Bytes<'a> {
    dir: BorrowedFd<'a>,
    name: String,
    path: PathBuf,
}

impl<'a> Bytes<'a> {
    /// The file `name` of `dir`, which is reported as `dir_path`.
    fn new(dir: BorrowedFd<'a>, dir_path: &Path, name: String) -> Bytes<'a> {
        Bytes {
            dir,
            path: dir_path.join(&name),
            name,
        }
    }

    /// The file, open to read.
    fn open(&self) -> io::Result<File> {
        let fd = openat(self.dir, self.name.as_str(), READ_FLAGS, Mode::empty())?;
        Ok(File::from(fd))
    }

    /// The file, made new (owner read and write) and open to write.
    fn create(&self) -> io::Result<File> {
        let fd = openat(
            self.dir,
            self.name.as_str(),
            NEW_FLAGS,
            Mode::RUSR | Mode::WUSR,
        )?;
        Ok(File::from(fd))
    }
}

/// What the index says: an entry per path, in the order the bytes are numbered.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema: u32,
    entries: Vec<Entry>,
}

/// One path and what was at it: `None` when nothing was.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: PathBuf,
    was: Option<Kept>,
}

/// What was at a path before the update, when something was.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Kept {
    /// A regular file, whose bytes are the entry's file in the copy.
    File { mode: u32, uid: u32, gid: u32 },
    /// A symbolic link.
    Link { target: PathBuf, uid: u32, gid: u32 },
}

impl Kept {
    /// Whether `path`, which is `now`, is what was kept (a file's bytes kept as `bytes`).
    fn is_at(&self, path: &Path, now: &fs::Metadata, bytes: &Bytes<'_>) -> io::Result<bool> {
        let owner = (now.uid(), now.gid());
        match self {
            Kept::File { mode, uid, gid } => {
                let file_now = (now.file_type().is_file(), now.mode() & 0o7777, owner);
                if file_now != (true, *mode, (*uid, *gid)) {
                    return Ok(false);
                }
                same_bytes(&mut File::open(path)?, &mut bytes.open()?)
            }
            Kept::Link { target, uid, gid } => {
                let target_now = if now.file_type().is_symlink() {
                    Some(fs::read_link(path)?)
                } else {
                    None
                };
                Ok((target_now.as_deref(), owner) == (Some(target.as_path()), (*uid, *gid)))
            }
        }
    }

    /// Makes what was kept at `temp`, which is not there (a file's bytes kept as `bytes`).
    fn make_at(&self, temp: &Path, bytes: &Bytes<'_>) -> io::Result<()> {
        match self {
            Kept::File { mode, uid, gid } => write_file(temp, bytes, *mode, *uid, *gid),
            Kept::Link { target, uid, gid } => std::os::unix::fs::symlink(target, temp)
                .and_then(|()| lchown(temp, Some(*uid), Some(*gid))),
        }
    }
}

/// What `rollback` did with each path of the copy.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PutBack {
    /// Written back.
    pub written: Vec<PathBuf>,
    /// Already as it was before the update.
    pub same: Vec<PathBuf>,
    /// Nothing was there before the update; left as it is.
    pub absent: Vec<PathBuf>,
}

/// `paths` as the words of a detail.
fn listed(paths: &[PathBuf]) -> String {
    let words: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    words.join(", ")
}

impl PutBack {
    /// What the last line tells of it, each part ended by `"; "`.
    pub fn told(&self) -> String {
        let parts = [
            (&self.written, "put back as before the update: "),
            (&self.same, "already as before the update: "),
            (
                &self.absent,
                "not there before the update, so nothing to put back: ",
            ),
        ];
        parts
            .iter()
            .filter(|(paths, _)| !paths.is_empty())
            .map(|(paths, words)| format!("{words}{}; ", listed(paths)))
            .collect()
    }

    /// Whether the copy holds a file or a link: something the host had that it may lose.
    pub fn holds_any(&self) -> bool {
        self.written.len() + self.same.len() > 0
    }
}

/// `path` with its directory's links resolved, when the directory is there: one file reached
/// by two spellings (a Mac's `/etc` and `/private/etc`) is kept once.
fn host_path(path: &Path) -> PathBuf {
    let parent = path.parent().map(fs::canonicalize);
    match (parent, path.file_name()) {
        (Some(Ok(dir)), Some(name)) => dir.join(name),
        _ => path.to_owned(),
    }
}

/// Adds below `rel` every file and link of the directory `dir`, its subdirectories walked.
fn walk(dir: &Path, rel: &Path, out: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else {
            out.insert(rel);
        }
    }
    Ok(())
}

/// The host paths the package staged in `staged` places below `root`, and `flags_file`.
pub(super) fn placed(
    root: &Path,
    staged: &Path,
    flags_file: &Path,
) -> Result<Vec<PathBuf>, KeptError> {
    let tree = staged.join(TREE);
    let mut rels = BTreeSet::new();
    match walk(&tree, Path::new(""), &mut rels) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(at(&tree)(e)),
        _ => {}
    }
    let mut paths: BTreeSet<PathBuf> = rels.iter().map(|rel| host_path(&root.join(rel))).collect();
    paths.insert(host_path(flags_file));
    Ok(paths.into_iter().collect())
}

/// Syncs the host directory `dir`, so that what was renamed in it stays so.
fn sync_dir(dir: &Path) -> Result<(), KeptError> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(at(dir))
}

/// Removes the directory `name` of `parent`, reported as `path`, and what is in it, walking
/// down through descriptors without following a link; nothing when it is not there. A link or
/// a file at `name` is not removed: it is an error.
fn remove_tree(parent: BorrowedFd<'_>, name: &OsStr, path: &Path) -> Result<(), KeptError> {
    let dir = match openat(parent, name, DIR_FLAGS, Mode::empty()) {
        Err(Errno::NOENT) => return Ok(()),
        opened => opened.map_err(errno_at(path))?,
    };
    // The names first and the removals after: a directory read while it is changed may skip
    // an entry.
    let mut names = Vec::new();
    for entry in Dir::read_from(&dir).map_err(errno_at(path))? {
        let entry = entry.map_err(errno_at(path))?;
        let entry = OsStr::from_bytes(entry.file_name().to_bytes());
        if entry != "." && entry != ".." {
            names.push(entry.to_owned());
        }
    }
    for entry in names {
        let below = path.join(&entry);
        let seen =
            statat(&dir, entry.as_os_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(errno_at(&below))?;
        if FileType::from_raw_mode(seen.st_mode) == FileType::Directory {
            remove_tree(dir.as_fd(), entry.as_os_str(), &below)?;
        } else {
            unlinkat(&dir, entry.as_os_str(), AtFlags::empty()).map_err(errno_at(&below))?;
        }
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(errno_at(path))
}

/// Where the copy in `state_dir` is.
pub(super) fn dir_of(state_dir: &Path) -> PathBuf {
    state_dir.join(KEPT)
}

/// Whether there is a copy in `state`: anything at its name, a link not followed.
pub(super) fn exists(state: StateDir<'_>) -> Result<bool, KeptError> {
    match statat(state.fd, KEPT, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(false),
        seen => seen.map(|_| true).map_err(errno_at(&state.path.join(KEPT))),
    }
}

/// Syncs the state directory, so that what was renamed in it stays so.
fn sync_state(state: StateDir<'_>) -> Result<(), KeptError> {
    fsync(state.fd).map_err(errno_at(state.path))
}

/// Removes the copy in `state`, and what a killed step left of one.
pub(super) fn discard(state: StateDir<'_>) -> Result<(), KeptError> {
    let making = state.path.join(MAKING);
    remove_tree(state.fd, OsStr::new(MAKING), &making)?;
    if exists(state)? {
        let kept = state.path.join(KEPT);
        renameat(state.fd, KEPT, state.fd, MAKING).map_err(errno_at(&kept))?;
        sync_state(state)?;
        remove_tree(state.fd, OsStr::new(MAKING), &making)?;
    }
    Ok(())
}

/// What is at `path`, a link not followed; `None` when nothing is.
fn meta_at(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        meta => meta.map(Some),
    }
}

/// What is at `path` now, the bytes of a file written to `bytes`.
fn keep_one(path: &Path, bytes: &Bytes<'_>) -> Result<Option<Kept>, KeptError> {
    let Some(meta) = meta_at(path).map_err(at(path))? else {
        return Ok(None);
    };
    let (uid, gid) = (meta.uid(), meta.gid());
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(at(path))?;
        return Ok(Some(Kept::Link { target, uid, gid }));
    }
    if !meta.file_type().is_file() {
        return Err(KeptError::Refused {
            path: path.to_owned(),
            what: "is neither a file nor a link",
        });
    }
    // Opened without following a link, and the bytes copied from what was opened.
    let mut from = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(at(path))?;
    let mut to = bytes.create().map_err(at(&bytes.path))?;
    io::copy(&mut from, &mut to)
        .and_then(|_| to.sync_all())
        .map_err(at(&bytes.path))?;
    Ok(Some(Kept::File {
        mode: meta.mode() & 0o7777,
        uid,
        gid,
    }))
}

/// Writes `body` as the new file `file`, synced.
fn write_new(file: &Bytes<'_>, body: &[u8]) -> Result<(), KeptError> {
    let mut written = file.create().map_err(at(&file.path))?;
    io::Write::write_all(&mut written, body)
        .and_then(|()| written.sync_all())
        .map_err(at(&file.path))
}

/// Keeps what is at each of `paths` in `state`, as `kept/`, in place of a copy that is there.
pub(super) fn keep(state: StateDir<'_>, paths: &[PathBuf]) -> Result<(), KeptError> {
    discard(state)?;
    let making_path = state.path.join(MAKING);
    mkdirat(state.fd, MAKING, Mode::RWXU).map_err(errno_at(&making_path))?;
    let making = openat(state.fd, MAKING, DIR_FLAGS, Mode::empty());
    let making = making.map_err(errno_at(&making_path))?;
    let mut entries = Vec::with_capacity(paths.len());
    for (n, path) in paths.iter().enumerate() {
        let was = keep_one(
            path,
            &Bytes::new(making.as_fd(), &making_path, n.to_string()),
        )?;
        entries.push(Entry {
            path: path.clone(),
            was,
        });
    }
    let index = Index {
        schema: INDEX_SCHEMA,
        entries,
    };
    let file = Bytes::new(making.as_fd(), &making_path, INDEX.to_owned());
    let body = serde_json::to_vec_pretty(&index)
        .map_err(io::Error::other)
        .map_err(at(&file.path))?;
    write_new(&file, &body)?;
    fsync(&making).map_err(errno_at(&making_path))?;
    let kept = state.path.join(KEPT);
    renameat(state.fd, MAKING, state.fd, KEPT).map_err(errno_at(&kept))?;
    sync_state(state)
}

/// Whether the files `a` and `b` hold the same bytes.
fn same_bytes(a: &mut File, b: &mut File) -> io::Result<bool> {
    let (mut left, mut right) = (Vec::new(), Vec::new());
    a.read_to_end(&mut left)?;
    b.read_to_end(&mut right)?;
    Ok(left == right)
}

/// The name `path` is written under before it is renamed over it, and the directory it is in.
fn temp_of(path: &Path) -> Result<(PathBuf, &Path), KeptError> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(KeptError::Refused {
            path: path.to_owned(),
            what: "names no file in a directory",
        });
    };
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(RESTORE_SUFFIX);
    Ok((dir.join(temp), dir))
}

/// Writes the file kept as `bytes` at `temp`, its owner, group and mode as kept, synced.
fn write_file(temp: &Path, bytes: &Bytes<'_>, mode: u32, uid: u32, gid: u32) -> io::Result<()> {
    let mut from = bytes.open()?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(temp)?;
    io::copy(&mut from, &mut to)?;
    // The owner first: a change of owner clears the set-id bits a mode gives.
    std::os::unix::fs::fchown(&to, Some(uid), Some(gid))?;
    to.set_permissions(fs::Permissions::from_mode(mode))?;
    to.sync_all()
}

/// Puts what was kept back at `path` (a file's bytes kept as `bytes`); whether it wrote anything.
fn put_one(path: &Path, kept: &Kept, bytes: &Bytes<'_>) -> Result<bool, KeptError> {
    let now = meta_at(path).map_err(at(path))?;
    let same = match &now {
        Some(now) => kept.is_at(path, now, bytes).map_err(at(path))?,
        None => false,
    };
    if same {
        return Ok(false);
    }
    let (temp, dir) = temp_of(path)?;
    match fs::remove_file(&temp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(at(&temp)(e)),
        _ => {}
    }
    kept.make_at(&temp, bytes).map_err(at(&temp))?;
    fs::rename(&temp, path).map_err(at(path))?;
    sync_dir(dir)?;
    Ok(true)
}

/// Puts the copy in `state` back, path by path; `None` when there is no copy. The copy is left
/// where it is.
pub(super) fn put_back(state: StateDir<'_>) -> Result<Option<PutBack>, KeptError> {
    if !exists(state)? {
        return Ok(None);
    }
    let kept_path = state.path.join(KEPT);
    let kept = openat(state.fd, KEPT, DIR_FLAGS, Mode::empty()).map_err(errno_at(&kept_path))?;
    let file = Bytes::new(kept.as_fd(), &kept_path, INDEX.to_owned());
    let mut text = Vec::new();
    file.open()
        .and_then(|mut opened| opened.read_to_end(&mut text))
        .map_err(at(&file.path))?;
    let path = file.path;
    let index: Index = serde_json::from_slice(&text)
        .map_err(io::Error::other)
        .map_err(at(&path))?;
    if index.schema != INDEX_SCHEMA {
        return Err(KeptError::Refused {
            path,
            what: "is of a schema this hook does not read",
        });
    }
    let mut done = PutBack::default();
    for (n, entry) in index.entries.iter().enumerate() {
        let list = match &entry.was {
            None => &mut done.absent,
            Some(was) => {
                let bytes = Bytes::new(kept.as_fd(), &kept_path, n.to_string());
                match put_one(&entry.path, was, &bytes)? {
                    true => &mut done.written,
                    false => &mut done.same,
                }
            }
        };
        list.push(entry.path.clone());
    }
    Ok(Some(done))
}
