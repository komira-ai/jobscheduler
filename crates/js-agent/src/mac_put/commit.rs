//! The commit of one run's collected outputs to its put directory: `pack.part`, then
//! `pack.sha256-<hex>.part`, then `pack` (the module text says why in this order).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use sha2::{Digest as _, Sha256};
use tar::{Builder, Header, HeaderMode};

/// The name the node takes: the commit's last name.
const PACK: &str = "pack";

/// The name the bytes are written under: the node never takes a name ending in `.part`.
const PART: &str = "pack.part";

/// The mode of what is committed: the agent's to write, the node's group's to read (the
/// node runs as another user and needs to read the file to store it).
const PACK_MODE: u32 = 0o640;

/// What a commit put in the ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The object's key: the put directory's path below the put root, then `/pack`.
    pub key: String,
    /// The SHA-256 of its bytes, in lowercase hex, which the commit declared.
    pub sha256: String,
    /// How many bytes it holds.
    pub bytes: u64,
}

/// Where one run's outputs are committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutTarget {
    /// The run's put directory: `<put root>/cas/b/<build>/<nonce>`, absolute.
    pub dir: PathBuf,
    /// Its path below the put root, `cas/b/<build>/<nonce>`: the key without `/pack`.
    pub key_prefix: String,
}

/// A writer that hashes and counts what passes through it.
struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Commits `collected` to `target` as the object `<key_prefix>/pack`: a directory as one tar
/// of the regular files and directories under it, each at its path below `collected`; a
/// regular file as it is. `target.dir` is made when missing and must hold nothing; `ready` is
/// called on it before anything is written in it (on a Mac it gives the directory to the
/// node's group). On a failure `pack.part` and the declared name are removed and `pack` is
/// never made.
///
/// # Errors
///
/// `target.dir` already holds something, `ready` failed, something under `collected` is a
/// link or neither a regular file nor a directory (the error names its path), or a read, a
/// write or a rename failed.
pub fn commit(
    collected: &Path,
    target: &PutTarget,
    ready: &dyn Fn(&Path) -> io::Result<()>,
) -> io::Result<Committed> {
    commit_with(collected, target, ready, &mut |_| {})
}

/// [`commit`], calling `seen` with the put directory after each of its three names was made:
/// written (`pack.part`), declared (`pack.sha256-<hex>.part`) and published (`pack`).
pub(crate) fn commit_with(
    collected: &Path,
    target: &PutTarget,
    ready: &dyn Fn(&Path) -> io::Result<()>,
    seen: &mut dyn FnMut(&Path),
) -> io::Result<Committed> {
    let dir = &target.dir;
    fs::create_dir_all(dir)?;
    if let Some(held) = fs::read_dir(dir)?.next() {
        let name = held?.file_name();
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already holds {}", dir.display(), name.display()),
        ));
    }
    ready(dir)?;
    let part = dir.join(PART);
    let mut declared = None;
    let done = publish(collected, dir, &mut declared, seen);
    if done.is_err() {
        for made in [Some(part), declared].into_iter().flatten() {
            // What is left of a failed commit is a `.part` name, which the node never takes;
            // the error says the commit failed either way.
            let _ = fs::remove_file(made);
        }
    }
    let (sha256, bytes) = done?;
    Ok(Committed {
        key: format!("{}/{PACK}", target.key_prefix),
        sha256,
        bytes,
    })
}

/// Writes `collected` to `pack.part` in `dir`, then declares and publishes it; `declared`
/// holds the declared name once it is made. Answers the digest and the length.
fn publish(
    collected: &Path,
    dir: &Path,
    declared: &mut Option<PathBuf>,
    seen: &mut dyn FnMut(&Path),
) -> io::Result<(String, u64)> {
    let part = dir.join(PART);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&part)?;
    // The mode is set on the open file, not left to the umask: a file the node's group
    // cannot read is never stored and stays in the directory.
    file.set_permissions(Permissions::from_mode(PACK_MODE))?;
    let mut out = Hashing {
        inner: file,
        hash: Sha256::new(),
        bytes: 0,
    };
    match fs::symlink_metadata(collected)
        .map_err(|why| named(collected, why))?
        .is_dir()
    {
        true => out = tar_of(collected, out)?,
        false => {
            io::copy(&mut open_regular(collected)?, &mut out)?;
        }
    }
    out.flush()?;
    out.inner.sync_all()?;
    seen(dir);
    let sha256 = hex::encode(out.hash.finalize());
    let name = dir.join(format!("{PACK}.sha256-{sha256}.part"));
    fs::rename(&part, &name)?;
    *declared = Some(name.clone());
    seen(dir);
    fs::rename(&name, dir.join(PACK))?;
    seen(dir);
    Ok((sha256, out.bytes))
}

/// Writes the tar of the tree `collected` to `out` and answers `out`.
fn tar_of<W: Write>(collected: &Path, out: W) -> io::Result<W> {
    let mut tar = Builder::new(out);
    // No owner, no time, and every file as a plain entry, holes or not: the same outputs
    // make the same bytes, and so the same digest.
    tar.mode(HeaderMode::Deterministic);
    tar.sparse(false);
    tar.follow_symlinks(false);
    append_tree(&mut tar, collected, Path::new(""))?;
    tar.into_inner()
}

/// Appends what the directory `dir` holds, under `below` in the tar, in name order.
fn append_tree<W: Write>(tar: &mut Builder<W>, dir: &Path, below: &Path) -> io::Result<()> {
    let listed = fs::read_dir(dir).and_then(Iterator::collect::<io::Result<Vec<_>>>);
    let entries = listed.map_err(|why| named(dir, why))?;
    let mut names: Vec<OsString> = entries.iter().map(fs::DirEntry::file_name).collect();
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let inside = below.join(&name);
        let meta = fs::symlink_metadata(&path).map_err(|why| named(&path, why))?;
        if meta.is_dir() {
            let mut header = Header::new_gnu();
            header.set_metadata_in_mode(&meta, HeaderMode::Deterministic);
            tar.append_data(&mut header, &inside, io::empty())?;
            append_tree(tar, &path, &inside)?;
        } else if meta.is_symlink() {
            return Err(refused(&path, "a link, not followed"));
        } else {
            tar.append_file(&inside, &mut open_regular(&path)?)?;
        }
    }
    Ok(())
}

/// Opens `path` to read when it is a regular file, never through a link.
///
/// # Errors
///
/// `path` is a link (`O_NOFOLLOW`), neither a regular file nor a directory, or cannot be
/// opened; the error names it.
fn open_regular(path: &Path) -> io::Result<File> {
    // `NONBLOCK`: opening a pipe someone left there does not wait for a writer.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let opened = rustix::fs::open(path, flags, Mode::empty());
    let file = File::from(opened.map_err(|why| named(path, why.into()))?);
    match file.metadata()?.is_file() {
        true => Ok(file),
        false => Err(refused(path, "not a regular file or a directory")),
    }
}

/// The failure `why` of a call on `path`, of the same kind and naming it.
fn named(path: &Path, why: io::Error) -> io::Error {
    io::Error::new(why.kind(), format!("{}: {why}", path.display()))
}

/// The refusal of `path`, saying why and naming it.
fn refused(path: &Path, why: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}: {why}", path.display()),
    )
}
