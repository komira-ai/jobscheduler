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
//! A file is put back by writing a file beside it (`.<name>.js-agent-config-restore`), giving it
//! its owner, group and mode, syncing it and renaming it over the path, then syncing the
//! directory, so the path holds the file that was there or the kept one and never a part of
//! either; a link is made beside it and renamed the same way. A path that already holds what was
//! kept is not written.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, lchown};
use std::path::{Path, PathBuf};

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
    fn is_at(&self, path: &Path, now: &fs::Metadata, bytes: &Path) -> io::Result<bool> {
        let owner = (now.uid(), now.gid());
        match self {
            Kept::File { mode, uid, gid } => {
                let file_now = (now.file_type().is_file(), now.mode() & 0o7777, owner);
                if file_now != (true, *mode, (*uid, *gid)) {
                    return Ok(false);
                }
                same_bytes(&mut File::open(path)?, &mut File::open(bytes)?)
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
    fn make_at(&self, temp: &Path, bytes: &Path) -> io::Result<()> {
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

/// Syncs the directory `dir`, so that what was renamed or removed in it stays so.
fn sync_dir(dir: &Path) -> Result<(), KeptError> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(at(dir))
}

/// Removes the directory `dir` and what is in it; nothing when it is not there.
fn remove(dir: &Path) -> Result<(), KeptError> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(at(dir)(e)),
        _ => Ok(()),
    }
}

/// Where the copy in `state_dir` is.
pub(super) fn dir_of(state_dir: &Path) -> PathBuf {
    state_dir.join(KEPT)
}

/// Whether there is a copy in `state_dir`.
pub(super) fn exists(state_dir: &Path) -> Result<bool, KeptError> {
    let kept = state_dir.join(KEPT);
    kept.try_exists().map_err(at(&kept))
}

/// Removes the copy in `state_dir`, and what a killed step left of one.
pub(super) fn discard(state_dir: &Path) -> Result<(), KeptError> {
    let (kept, making) = (state_dir.join(KEPT), state_dir.join(MAKING));
    remove(&making)?;
    if exists(state_dir)? {
        fs::rename(&kept, &making).map_err(at(&kept))?;
        sync_dir(state_dir)?;
        remove(&making)?;
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
fn keep_one(path: &Path, bytes: &Path) -> Result<Option<Kept>, KeptError> {
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
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(bytes)
        .map_err(at(bytes))?;
    io::copy(&mut from, &mut to)
        .and_then(|_| to.sync_all())
        .map_err(at(bytes))?;
    Ok(Some(Kept::File {
        mode: meta.mode() & 0o7777,
        uid,
        gid,
    }))
}

/// Writes `body` as the new file `path`, synced.
fn write_new(path: &Path, body: &[u8]) -> Result<(), KeptError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(at(path))?;
    io::Write::write_all(&mut file, body)
        .and_then(|()| file.sync_all())
        .map_err(at(path))
}

/// Keeps what is at each of `paths` in `state_dir`, as `kept/`, in place of a copy that is
/// there.
pub(super) fn keep(state_dir: &Path, paths: &[PathBuf]) -> Result<(), KeptError> {
    discard(state_dir)?;
    let making = state_dir.join(MAKING);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&making)
        .map_err(at(&making))?;
    let mut entries = Vec::with_capacity(paths.len());
    for (n, path) in paths.iter().enumerate() {
        let was = keep_one(path, &making.join(n.to_string()))?;
        entries.push(Entry {
            path: path.clone(),
            was,
        });
    }
    let index = Index {
        schema: INDEX_SCHEMA,
        entries,
    };
    let path = making.join(INDEX);
    let body = serde_json::to_vec_pretty(&index)
        .map_err(io::Error::other)
        .map_err(at(&path))?;
    write_new(&path, &body)?;
    sync_dir(&making)?;
    let kept = state_dir.join(KEPT);
    fs::rename(&making, &kept).map_err(at(&kept))?;
    sync_dir(state_dir)
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
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name);
    temp.push(RESTORE_SUFFIX);
    Ok((dir.join(temp), dir))
}

/// Writes the file kept as `bytes` at `temp`, its owner, group and mode as kept, synced.
fn write_file(temp: &Path, bytes: &Path, mode: u32, uid: u32, gid: u32) -> io::Result<()> {
    let mut from = File::open(bytes)?;
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
fn put_one(path: &Path, kept: &Kept, bytes: &Path) -> Result<bool, KeptError> {
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

/// Puts the copy in `state_dir` back, path by path; `None` when there is no copy. The copy is
/// left where it is.
pub(super) fn put_back(state_dir: &Path) -> Result<Option<PutBack>, KeptError> {
    if !exists(state_dir)? {
        return Ok(None);
    }
    let kept = state_dir.join(KEPT);
    let path = kept.join(INDEX);
    let text = fs::read(&path).map_err(at(&path))?;
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
            Some(was) => match put_one(&entry.path, was, &kept.join(n.to_string()))? {
                true => &mut done.written,
                false => &mut done.same,
            },
        };
        list.push(entry.path.clone());
    }
    Ok(Some(done))
}
