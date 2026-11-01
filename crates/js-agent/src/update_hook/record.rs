//! The hook record: what a step did, kept where the next step (or a rerun of the same one)
//! finds it, and the lock that keeps two steps apart (host-updates design, 4.2, "The hook
//! record" and "The record directory is not the service's").
//!
//! The record is `state_dir/record.json`; the lock is `state_dir/lock`, an exclusive `flock`
//! taken without waiting and held for the whole step, so a second copy refuses (exit 1,
//! reason `locked`) having changed nothing. The directory must be `root:root`, mode 0700 and
//! not a symlink, and every directory above it must be owned by root and not writable by its
//! group or by others, or the step refuses (`state_invalid`) before it opens anything in it.
//!
//! The directory is opened once, walking down from `/` one component at a time without
//! following a link, and checked through the descriptor it was opened as. The lock, the
//! record and the temporary file are opened, renamed and synced relative to that descriptor,
//! never by path again, so a path swapped for a link after the open redirects nothing.
//!
//! A save writes a temporary file in the directory, syncs it, renames it over the record and
//! syncs the directory, so the record is the old one or the new one and never a part of
//! either. A record whose schema is newer than [`SCHEMA`] is read-only: it is not loaded, and
//! it is not overwritten.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fsync, openat, renameat, statat,
    unlinkat,
};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use super::plain::is_plain;
use super::report::Reason;
use super::validate::Invalid;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// The record, directly in the state directory.
pub const RECORD_FILE: &str = "record.json";

/// The file a save writes through, renamed over [`RECORD_FILE`].
pub const TEMP_FILE: &str = "record.json.tmp";

/// The lock, directly in the state directory.
pub const LOCK_FILE: &str = "lock";

/// The most bytes of a record that are read. A real record is under a kilobyte.
pub const RECORD_MAX: u64 = 64 * 1024;

/// Which update the record is of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInfo {
    /// The version being installed.
    pub target: String,
    /// The version that runs now; `None` on a first install.
    pub previous: Option<String>,
    /// The staged release directory of `target`; the table's `updater.staged_release`.
    pub staged: PathBuf,
    /// The release directory the link pointed to before the update; `None` on a first install.
    pub previous_target: Option<PathBuf>,
    /// The unit that runs the agent.
    pub unit: String,
    /// The digest of the artifact the staged release was made from, `sha256:<hex>`.
    pub artifact_digest: String,
}

/// The cordon this update placed on the host, if it did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cordon {
    /// Whether this update placed it. A cordon that was there before is not the hook's to lift.
    pub placed: bool,
    /// The mark was written before the cordon was asked of the lease server and no reply has
    /// confirmed it: a cordon found in force is then not known to be ours. Cleared by a reply
    /// that says the host was not cordoned.
    #[serde(default)]
    pub provisional: bool,
    /// When it was first placed, in milliseconds on the wall clock; 0 when not placed.
    pub first_ms: u64,
    /// When it now expires, in whole seconds' worth of milliseconds: never later than
    /// `first_ms + cordon_max_secs * 1000` plus the one second the validator allows
    /// ([`EXPIRY_SLACK_MS`](super::validate::EXPIRY_SLACK_MS)). 0 when not placed.
    pub expires_ms: u64,
}

/// The marks of `rollback`'s sub-steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    /// The bounded wait saw no lease.
    pub drained: bool,
    /// The stop was begun (written before the signal).
    pub stop_begun: bool,
    /// The link was moved back to the previous release.
    pub link_restored: bool,
    /// The unit was started again.
    pub start_issued: bool,
    /// The previous version's hello was seen.
    pub hello_ok: bool,
}

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The format of this file; see [`SCHEMA`].
    pub schema: u32,
    /// Which update.
    pub update: UpdateInfo,
    /// The cordon.
    pub cordon: Cordon,
    /// The stop is begun: written and synced before `cutover` signals the unit, so a
    /// `cutover` killed at any later point leaves it set.
    pub stop_begun: bool,
    /// The `session_started_ms` the lease server showed for the host in the reading `cutover`
    /// took last before it marked the stop, on the server's clock; `None` on a first install
    /// and when the server showed no session. Written in the same save as `stop_begun`, never
    /// overwritten by a rerun, and the only thing `verify` compares a new session with: two
    /// stamps of the one clock, never one clock with another. Left out of the file when
    /// `None`, so such a record reads as it did before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_stop_session_ms: Option<u64>,
    /// The move of the link to the staged release is begun: written before the move, so a
    /// `cutover` killed at any later point leaves it set. The link itself says whether it
    /// happened.
    pub link_moved: bool,
    /// The start of the unit on the staged release is begun: written before the start. The
    /// unit manager says whether it happened.
    pub start_issued: bool,
    /// A `verify` succeeded.
    pub verify_ok: bool,
    /// What `rollback` has done.
    pub rb: Rollback,
}

impl Record {
    /// The record of a `prepare` that has just begun: nothing done.
    #[must_use]
    pub fn new(update: UpdateInfo) -> Record {
        Record {
            schema: SCHEMA,
            update,
            cordon: Cordon::default(),
            stop_begun: false,
            pre_stop_session_ms: None,
            link_moved: false,
            start_issued: false,
            verify_ok: false,
            rb: Rollback::default(),
        }
    }
}

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

/// Why the state could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
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
    /// The lock could not be taken: another step holds it.
    #[error("cannot lock {}: {source}", path.display())]
    Lock {
        /// The lock file.
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
    #[error("the record has schema {found}, newer than {SCHEMA}: read-only to this hook")]
    Newer {
        /// The record's.
        found: u32,
    },
    /// The record breaks a rule of the declaration.
    #[error("the record is not valid: {0}")]
    Invalid(#[from] Invalid),
}

impl StateError {
    /// The reason the step's last line gives: `locked` for a second holder, `state_invalid`
    /// for everything else.
    #[must_use]
    pub fn reason(&self) -> Reason {
        match self {
            StateError::Lock { .. } => Reason::Locked,
            _ => Reason::StateInvalid,
        }
    }
}

/// An `Io` error at `path`, for `map_err`.
fn at(path: &Path) -> impl FnOnce(io::Error) -> StateError + '_ {
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
fn refusal(path: &Path, last: bool, refused: Refused) -> StateError {
    let path = path.to_owned();
    match refused {
        Refused::Io(source) => StateError::Io { path, source },
        Refused::Problem(problem) if last => StateError::Dir { path, problem },
        Refused::Problem(problem) => StateError::Ancestor { path, problem },
    }
}

/// Opens `dir` by walking down from `/`, one component at a time and without following a
/// link, holding each ancestor to [`check_ancestor`] before going into it and the last
/// directory to [`check_dir`]. Every look is at an open descriptor, never at a path, so what
/// is checked is what is returned.
fn open_checked(
    dir: &Path,
    stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
) -> Result<OwnedFd, StateError> {
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
pub struct Held {
    dir: PathBuf,
    fd: OwnedFd,
    lock: File,
}

impl Held {
    /// Opens `dir` once and takes the lock, without waiting. Every directory from `/` down
    /// is looked at through `stat` (see [`fd_stat`]) as it is opened, with its path.
    ///
    /// # Errors
    /// [`StateError::Dir`] if the directory is not `root:root` 0700 and not a link or the
    /// path is not plain, [`StateError::Ancestor`] if a directory above it is a link, not
    /// root's or writable by group or others, [`StateError::Lock`] if another step holds the
    /// lock, [`StateError::Io`] otherwise.
    pub fn open(
        dir: &Path,
        stat: &dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    ) -> Result<Held, StateError> {
        let fd = open_checked(dir, stat)?;
        let path = dir.join(LOCK_FILE);
        let lock = open_regular(fd.as_fd(), LOCK_FILE, OFlags::WRONLY | OFlags::CREATE)
            .map_err(at(&path))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|errno| {
            StateError::Lock {
                path,
                source: errno.into(),
            }
        })?;
        Ok(Held {
            dir: dir.to_owned(),
            fd,
            lock,
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
    pub fn load(&self) -> Result<Option<Record>, StateError> {
        let path = self.record_path();
        let mut bytes = Vec::new();
        // Without following a link and without waiting for a writer: a pipe put in the
        // record's place is refused below, not waited on.
        match open_in(self.fd.as_fd(), RECORD_FILE, OFlags::RDONLY) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(at(&path)(source)),
            Ok(file) => {
                regular(&file).map_err(|e| StateError::Corrupt(e.to_string()))?;
                file.take(RECORD_MAX + 1)
                    .read_to_end(&mut bytes)
                    .map_err(at(&path))?
            }
        };
        if bytes.len() as u64 > RECORD_MAX {
            return Err(StateError::Corrupt(format!("more than {RECORD_MAX} bytes")));
        }
        let header: Header =
            serde_json::from_slice(&bytes).map_err(|e| StateError::Corrupt(e.to_string()))?;
        match header.schema.cmp(&SCHEMA) {
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

    /// Makes `record` the record: written, synced, renamed over the old one, the directory
    /// synced. Refuses to replace a record [`Held::load`] refuses.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`].
    pub fn save(&self, record: &Record) -> Result<(), StateError> {
        self.write_temp(record)?;
        self.commit()
    }

    /// The first half of a save: `record` written to the temporary file and synced. Public
    /// so a test can stop here, as a crash does; the record is still the old one. Like the
    /// whole save, it refuses when the record that is there cannot be loaded.
    ///
    /// # Errors
    /// What [`Held::load`] says of the record that is there, or [`StateError::Io`].
    pub fn write_temp(&self, record: &Record) -> Result<(), StateError> {
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
    pub fn commit(&self) -> Result<(), StateError> {
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
    pub fn delete(&self) -> Result<(), StateError> {
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

impl Drop for Held {
    fn drop(&mut self) {
        let _ = flock(&self.lock, FlockOperation::Unlock);
    }
}
