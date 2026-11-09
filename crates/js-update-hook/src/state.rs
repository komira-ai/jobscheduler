//! The state directory of a hook and the record in it: what a step did, kept where the next
//! step (or a rerun of the same one) finds it, and the lock that keeps two steps apart
//! (host-updates design, 4.2, "The hook record" and "The record directory is not the
//! service's").
//!
//! The record is `state_dir/record.json`, a [`Document`] of the hook's own. The lock is an
//! exclusive `flock` on `state_dir` itself (the hook contract's lock) and one on
//! `state_dir/lock` (kept so that a hook built before the directory lock still excludes this
//! one). Both are taken without waiting and held for the whole step, the directory's first, so
//! a second copy, or anything that holds the directory, makes the step refuse (exit 1, reason
//! [`Document::LOCKED`]) having changed nothing. The directory must be `root:root`, mode 0700
//! and not a symlink, and every directory above it must be owned by root and not writable by
//! its group or by others, or the step refuses ([`Document::STATE_INVALID`]) before it opens
//! anything in it.
//!
//! hostkeeper makes no state directory for a hook: it passes `state_dir` on as one more value
//! of the manifest's `[params]` and never looks at it. [`Held::open_or_make`] therefore makes it,
//! mode 0700, when it is not there, and holds it to the same rules: only the directory itself,
//! never one above it, made in its parent through the descriptor the walk checked (so a link
//! put in its place is never followed), the parent synced so the new entry survives a crash.
//! [`Held::open`] makes nothing.
//!
//! The directory is opened once, walking down from `/` one component at a time without
//! following a link, and checked through the descriptor it was opened as. The lock, the
//! record and the temporary file are opened, renamed and synced relative to that descriptor,
//! never by path again, so a path swapped for a link after the open redirects nothing.
//!
//! A save writes a temporary file in the directory, syncs it, renames it over the record and
//! syncs the directory, so the record is the old one or the new one and never a part of
//! either. A record whose schema is newer than [`Document::SCHEMA`] is read-only: it is not
//! loaded, and it is not overwritten.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fsync, mkdirat, openat, renameat,
    statat, unlinkat,
};
use rustix::io::Errno;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::plain::is_plain;
use crate::report::ReasonToken;

/// A hook's record: what it keeps in the state directory, in the schema it writes.
pub trait Document: Serialize + DeserializeOwned {
    /// The schema this hook writes and reads. A record of a newer one is read-only to the
    /// hook ([`StateError::Newer`]); one of an older one is refused ([`StateError::Corrupt`]).
    const SCHEMA: u32;

    /// What the hook's own rules say of a record they refuse ([`StateError::Invalid`]).
    type Invalid: std::error::Error + 'static;

    /// The hook's reasons.
    type Reason: ReasonToken;

    /// The reason of a lock that another holder has ([`StateError::Lock`]).
    const LOCKED: Self::Reason;

    /// The reason of every other [`StateError`].
    const STATE_INVALID: Self::Reason;
}

/// The record, directly in the state directory.
pub const RECORD_FILE: &str = "record.json";

/// The file a save writes through, renamed over [`RECORD_FILE`].
pub const TEMP_FILE: &str = "record.json.tmp";

/// The lock, directly in the state directory.
pub const LOCK_FILE: &str = "lock";

/// The most bytes of a record that are read. A real record is under a kilobyte.
pub const RECORD_MAX: u64 = 64 * 1024;

/// What the check of the state directory looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirStat {
    /// The path itself is a symbolic link (it is not followed).
    pub is_symlink: bool,
    /// The path is a directory.
    pub is_dir: bool,
    /// The owner.
    pub uid: u32,
    /// The group.
    pub gid: u32,
    /// The permission bits, `0o7777` of the mode.
    pub mode: u32,
}

/// The open directory `fd` as the check sees it. The directory was opened without following
/// a link, so a link is never what is looked at.
///
/// # Errors
/// The descriptor cannot be looked at.
pub fn fd_stat(fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    let meta = File::from(fd.try_clone_to_owned()?).metadata()?;
    Ok(DirStat {
        is_symlink: false,
        is_dir: meta.is_dir(),
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
    })
}

/// Why the state directory is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DirProblem {
    /// It is a symbolic link.
    #[error("is a symbolic link")]
    Symlink,
    /// It is not a directory.
    #[error("is not a directory")]
    NotDirectory,
    /// It is not owned by `root:root`.
    #[error("is owned by {uid}:{gid}, not 0:0")]
    Owner {
        /// The owner.
        uid: u32,
        /// The group.
        gid: u32,
    },
    /// Its mode is not 0700.
    #[error("has mode {mode:04o}, not 0700")]
    Mode {
        /// The permission bits.
        mode: u32,
    },
    /// The path is not absolute and plain.
    #[error("is not an absolute path of plain components")]
    NotPlain,
    /// An ancestor is not owned by root.
    #[error("is owned by {uid}, not by root")]
    NotRootOwned {
        /// The owner.
        uid: u32,
    },
    /// An ancestor can be written by its group or by others.
    #[error("has mode {mode:04o}, writable by its group or by others")]
    Writable {
        /// The permission bits.
        mode: u32,
    },
}

/// Holds an ancestor of the state directory to: a directory, not a link, owned by root, not
/// writable by its group or by others.
///
/// # Errors
/// The first [`DirProblem`] it shows.
pub fn check_ancestor(stat: &DirStat) -> Result<(), DirProblem> {
    if stat.is_symlink {
        Err(DirProblem::Symlink)
    } else if !stat.is_dir {
        Err(DirProblem::NotDirectory)
    } else if stat.uid != 0 {
        Err(DirProblem::NotRootOwned { uid: stat.uid })
    } else if stat.mode & 0o022 != 0 {
        Err(DirProblem::Writable { mode: stat.mode })
    } else {
        Ok(())
    }
}

/// Holds `stat` to `root:root`, mode 0700, a directory, not a link.
///
/// # Errors
/// The first [`DirProblem`] it shows.
pub fn check_dir(stat: &DirStat) -> Result<(), DirProblem> {
    if stat.is_symlink {
        Err(DirProblem::Symlink)
    } else if !stat.is_dir {
        Err(DirProblem::NotDirectory)
    } else if (stat.uid, stat.gid) != (0, 0) {
        Err(DirProblem::Owner {
            uid: stat.uid,
            gid: stat.gid,
        })
    } else if stat.mode != 0o700 {
        Err(DirProblem::Mode { mode: stat.mode })
    } else {
        Ok(())
    }
}

/// Why the state of a hook whose record is `D` could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StateError<D: Document> {
    /// The state directory is not as it must be.
    #[error("state directory {}: {problem}", path.display())]
    Dir {
        /// The directory.
        path: PathBuf,
        /// What is wrong.
        problem: DirProblem,
    },
    /// A directory above the state directory is not as it must be.
    #[error("above the state directory, {}: {problem}", path.display())]
    Ancestor {
        /// The ancestor.
        path: PathBuf,
        /// What is wrong.
        problem: DirProblem,
    },
    /// A lock could not be taken: another step, or anything else that locks the directory,
    /// holds it.
    #[error("cannot lock {}: {source}", path.display())]
    Lock {
        /// The state directory (the hook contract's lock) or the lock file in it, whichever
        /// was held.
        path: PathBuf,
        /// The system's answer; `EWOULDBLOCK` when another holder has it.
        source: io::Error,
    },
    /// A file operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// The record is not a record.
    #[error("the record is not readable: {0}")]
    Corrupt(String),
    /// The record is of a newer schema than this hook writes.
    #[error("the record has schema {found}, newer than {schema}: read-only to this hook", schema = D::SCHEMA)]
    Newer {
        /// The record's.
        found: u32,
    },
    /// The record breaks a rule of the hook's own (for js-agent, of its declaration).
    #[error("the record is not valid: {0}")]
    Invalid(#[source] D::Invalid),
}

impl<D: Document> StateError<D> {
    /// The reason the step's last line gives: [`Document::LOCKED`] for a second holder,
    /// [`Document::STATE_INVALID`] for everything else.
    #[must_use]
    pub fn reason(&self) -> D::Reason {
        match self {
            StateError::Lock { .. } => D::LOCKED,
            _ => D::STATE_INVALID,
        }
    }
}

/// An `Io` error at `path`, for `map_err`.
fn at<D: Document>(path: &Path) -> impl FnOnce(io::Error) -> StateError<D> + '_ {
    |source| StateError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Only the schema of a record, read before the rest is trusted.
#[derive(Deserialize)]
struct Header {
    schema: u32,
}

/// How a directory is opened: not a link, a directory, not inherited by a child.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Why a directory could not be opened.
enum Refused {
    /// What it is, when that is the reason.
    Problem(DirProblem),
    /// The system's answer otherwise.
    Io(io::Error),
}

/// Opens the directory `name` of `parent` without following a link.
fn open_dir_at(parent: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Refused> {
    match openat(parent, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(fd),
        // Which of the two a link gives depends on the system; ask what it is.
        Err(Errno::LOOP | Errno::NOTDIR) => {
            let link = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|seen| FileType::from_raw_mode(seen.st_mode) == FileType::Symlink);
            Err(Refused::Problem(if link {
                DirProblem::Symlink
            } else {
                DirProblem::NotDirectory
            }))
        }
        Err(errno) => Err(Refused::Io(errno.into())),
    }
}

/// The refusal of `path`, which is the state directory itself when `last` and one of its
/// ancestors otherwise.
fn refusal<D: Document>(path: &Path, last: bool, refused: Refused) -> StateError<D> {
    let path = path.to_owned();
    match refused {
        Refused::Io(source) => StateError::Io { path, source },
        Refused::Problem(problem) if last => StateError::Dir { path, problem },
        Refused::Problem(problem) => StateError::Ancestor { path, problem },
    }
}

/// Makes the directory `name` of `parent`, mode 0700, unless something is at that name already
/// (what it is, the open after this judges), and syncs `parent` so that the new entry survives a
/// crash.
fn make_dir_at(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) => fsync(parent).map_err(io::Error::from),
        Err(Errno::EXIST) => Ok(()),
        Err(errno) => Err(errno.into()),
    }
}

/// Opens `dir` by walking down from `/`, one component at a time and without following a
/// link, holding each ancestor to [`check_ancestor`] before going into it and the last
/// directory to [`check_dir`]. Every look is at an open descriptor, never at a path, so what
/// is checked is what is returned. With `make`, the last directory is made in its checked
/// parent first when nothing is at its name ([`make_dir_at`]).
fn open_checked<D: Document>(
    dir: &Path,
    stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    make: bool,
) -> Result<OwnedFd, StateError<D>> {
    if !is_plain(dir) {
        return Err(StateError::Dir {
            path: dir.to_owned(),
            problem: DirProblem::NotPlain,
        });
    }
    let mut here = PathBuf::from("/");
    let mut fd = openat(rustix::fs::CWD, "/", DIR_FLAGS, Mode::empty())
        .map_err(io::Error::from)
        .map_err(at(&here))?;
    let mut parts = dir.components().skip(1);
    loop {
        let seen = stat(&here, fd.as_fd()).map_err(at(&here))?;
        let Some(part) = parts.next() else {
            return check_dir(&seen)
                .map(|()| fd)
                .map_err(|problem| StateError::Dir {
                    path: here.clone(),
                    problem,
                });
        };
        check_ancestor(&seen).map_err(|problem| StateError::Ancestor {
            path: here.clone(),
            problem,
        })?;
        here.push(part);
        let last = parts.clone().next().is_none();
        if last && make {
            make_dir_at(fd.as_fd(), part.as_os_str()).map_err(at(&here))?;
        }
        fd = open_dir_at(fd.as_fd(), part.as_os_str())
            .map_err(|refused| refusal(&here, last, refused))?;
    }
}

/// Opens the file `name` of the directory `dir` without following a link and without waiting
/// for a writer or reader.
fn open_in(dir: BorrowedFd<'_>, name: &str, flags: OFlags) -> io::Result<File> {
    let flags = flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = openat(dir, name, flags, Mode::RUSR | Mode::WUSR)?;
    Ok(File::from(fd))
}

/// Whether `file`, as opened, is a regular file: a FIFO or a directory put in a file's place
/// is not.
fn regular(file: &File) -> io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(io::Error::other("not a regular file"))
    }
}

/// [`open_in`] for a file that is written: it must be a regular file once open.
fn open_regular(dir: BorrowedFd<'_>, name: &str, flags: OFlags) -> io::Result<File> {
    let file = open_in(dir, name, flags)?;
    regular(&file)?;
    Ok(file)
}

/// The state directory, checked and locked. The lock is released when this is dropped.
///
/// It is released with an explicit unlock, not by closing the file: a lock belongs to the
/// open file description, which a child forked by another thread of this process holds a
/// copy of until it execs, so closing alone can leave the directory locked for a moment.
///
/// The directory is held open as a descriptor; `dir` is only the name it is reported by.
#[derive(Debug)]
pub struct Held<D> {
    dir: PathBuf,
    fd: OwnedFd,
    lock: File,
    /// The record kept in the directory.
    record: PhantomData<fn() -> D>,
}

impl<D: Document> Held<D> {
    /// Opens `dir` once and takes the two locks, the directory's and then the lock file's,
    /// without waiting. Every directory from `/` down is looked at through `stat` (see
    /// [`fd_stat`]) as it is opened, with its path.
    ///
    /// # Errors
    /// [`StateError::Dir`] if the directory is not `root:root` 0700 and not a link or the
    /// path is not plain, [`StateError::Ancestor`] if a directory above it is a link, not
    /// root's or writable by group or others, [`StateError::Lock`] if another step holds the
    /// lock, [`StateError::Io`] otherwise.
    pub fn open(
        dir: &Path,
        stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    ) -> Result<Held<D>, StateError<D>> {
        Held::open_dir(dir, stat, false)
    }

    /// [`Held::open`], having first made `dir` (mode 0700, owned by the user the hook runs as)
    /// when nothing is at its path: the first step of a first install under hostkeeper finds no
    /// directory, since hostkeeper makes none. Only `dir` itself is made, in the parent the walk checked;
    /// a directory above it that is not there is still [`StateError::Io`] naming it. What is made
    /// is held to the same rules as a directory that was there.
    ///
    /// # Errors
    /// As [`Held::open`]; [`StateError::Io`] naming `dir` when it cannot be made.
    pub fn open_or_make(
        dir: &Path,
        stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    ) -> Result<Held<D>, StateError<D>> {
        Held::open_dir(dir, stat, true)
    }

    /// [`Held::open`], or [`Held::open_or_make`] with `make`.
    fn open_dir(
        dir: &Path,
        stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
        make: bool,
    ) -> Result<Held<D>, StateError<D>> {
        let fd = open_checked(dir, stat, make)?;
        // The directory first: a holder of it (the updater, another step) is refused before
        // anything is made in it.
        flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|errno| StateError::Lock {
            path: dir.to_owned(),
            source: errno.into(),
        })?;
        let path = dir.join(LOCK_FILE);
        let locked = open_regular(fd.as_fd(), LOCK_FILE, OFlags::WRONLY | OFlags::CREATE)
            .map_err(at(&path))
            .and_then(|lock| {
                flock(&lock, FlockOperation::NonBlockingLockExclusive)
                    .map(|()| lock)
                    .map_err(|errno| StateError::Lock {
                        path,
                        source: errno.into(),
                    })
            });
        let lock = match locked {
            Ok(lock) => lock,
            Err(error) => {
                // Released explicitly, as `Held`'s `drop` releases it: a copy of the descriptor
                // in a child forked meanwhile would keep the lock after the close.
                let _ = flock(&fd, FlockOperation::Unlock);
                return Err(error);
            }
        };
        Ok(Held {
            dir: dir.to_owned(),
            fd,
            lock,
            record: PhantomData,
        })
    }

    /// The record's path.
    #[must_use]
    pub fn record_path(&self) -> PathBuf {
        self.dir.join(RECORD_FILE)
    }

    /// The record, `None` if there is none.
    ///
    /// # Errors
    /// [`StateError::Newer`] for a newer schema, [`StateError::Corrupt`] for anything that is
    /// not a record of this one, [`StateError::Io`] if the file cannot be read.
    pub fn load(&self) -> Result<Option<D>, StateError<D>> {
        load_in(self.fd.as_fd(), &self.record_path())
    }

    /// Makes `record` the record: written, synced, renamed over the old one, the directory
    /// synced. Refuses to replace a record [`Held::load`] refuses.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`].
    pub fn save(&self, record: &D) -> Result<(), StateError<D>> {
        self.write_temp(record)?;
        self.commit()
    }

    /// The first half of a save: `record` written to the temporary file and synced. Public
    /// so a test can stop here, as a crash does; the record is still the old one. Like the
    /// whole save, it refuses when the record that is there cannot be loaded.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`].
    pub fn write_temp(&self, record: &D) -> Result<(), StateError<D>> {
        self.load()?;
        let temp = self.dir.join(TEMP_FILE);
        let mut body = serde_json::to_vec_pretty(record)
            .map_err(io::Error::other)
            .map_err(at(&temp))?;
        body.push(b'\n');
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC;
        let mut file = open_regular(self.fd.as_fd(), TEMP_FILE, flags).map_err(at(&temp))?;
        file.write_all(&body)
            .and_then(|()| file.sync_all())
            .map_err(at(&temp))
    }

    /// The second half of a save: the temporary file renamed over the record, the directory
    /// synced.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`], for
    /// instance when there is no temporary file.
    pub fn commit(&self) -> Result<(), StateError<D>> {
        self.load()?;
        let record = self.record_path();
        renameat(&self.fd, TEMP_FILE, &self.fd, RECORD_FILE)
            .map_err(io::Error::from)
            .map_err(at(&record))?;
        fsync(&self.fd)
            .map_err(io::Error::from)
            .map_err(at(&self.dir))
    }

    /// Deletes the record, and a temporary file a killed save left, and syncs the directory.
    /// Does nothing to a directory with no record. Refuses to delete a record [`Held::load`]
    /// refuses, as a save does.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`].
    pub fn delete(&self) -> Result<(), StateError<D>> {
        self.load()?;
        // The temporary file first: a delete killed between the two leaves the record, which
        // the next step deletes again.
        for name in [TEMP_FILE, RECORD_FILE] {
            match unlinkat(&self.fd, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(errno) => return Err(at(&self.dir.join(name))(errno.into())),
            }
        }
        fsync(&self.fd)
            .map_err(io::Error::from)
            .map_err(at(&self.dir))
    }
}

/// The record in the directory `dir`, reported as `path`; `None` if there is none (see
/// [`Held::load`]).
fn load_in<D: Document>(dir: BorrowedFd<'_>, path: &Path) -> Result<Option<D>, StateError<D>> {
    let mut bytes = Vec::new();
    // Without following a link and without waiting for a writer: a pipe put in the
    // record's place is refused below, not waited on.
    match open_in(dir, RECORD_FILE, OFlags::RDONLY) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(at(path)(source)),
        Ok(file) => {
            regular(&file).map_err(|e| StateError::Corrupt(e.to_string()))?;
            file.take(RECORD_MAX + 1)
                .read_to_end(&mut bytes)
                .map_err(at(path))?
        }
    };
    if bytes.len() as u64 > RECORD_MAX {
        return Err(StateError::Corrupt(format!("more than {RECORD_MAX} bytes")));
    }
    let header: Header =
        serde_json::from_slice(&bytes).map_err(|e| StateError::Corrupt(e.to_string()))?;
    match header.schema.cmp(&D::SCHEMA) {
        std::cmp::Ordering::Greater => Err(StateError::Newer {
            found: header.schema,
        }),
        std::cmp::Ordering::Less => Err(StateError::Corrupt(format!(
            "schema {} is not one this hook wrote",
            header.schema
        ))),
        std::cmp::Ordering::Equal => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StateError::Corrupt(e.to_string())),
    }
}

/// The record in `dir` as it is now, `None` if there is none, read without the lock and
/// without creating or changing anything: the directory is opened and held to the rules as
/// [`Held::open`] does, and the record read as [`Held::load`] does. A save replaces the record
/// by a rename, so what is read is a whole record, the one before a save that runs meanwhile
/// or the one after it. It tells a step that will not run what the record says it has done.
///
/// # Errors
/// What [`Held::open`] says of the directory (except [`StateError::Lock`]: no lock is taken)
/// and what [`Held::load`] says of the record.
pub fn peek<D: Document>(
    dir: &Path,
    stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
) -> Result<Option<D>, StateError<D>> {
    let fd = open_checked(dir, stat, false)?;
    load_in(fd.as_fd(), &dir.join(RECORD_FILE))
}

impl<D> Drop for Held<D> {
    fn drop(&mut self) {
        let _ = flock(&self.lock, FlockOperation::Unlock);
        let _ = flock(&self.fd, FlockOperation::Unlock);
    }
}
