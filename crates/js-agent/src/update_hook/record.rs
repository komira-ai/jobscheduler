//! The hook record: what a step did, kept where the next step (or a rerun of the same one)
//! finds it, and the lock that keeps two steps apart (host-updates design, 4.2, "The hook
//! record" and "The record directory is not the service's").
//!
//! The record is `state_dir/record.json`; the lock is `state_dir/lock`, an exclusive `flock`
//! taken without waiting and held for the whole step, so a second copy refuses (exit 1,
//! reason `locked`) having changed nothing. The directory must be `root:root`, mode 0700 and
//! not a symlink, or the step refuses (`state_invalid`) before it opens anything in it.
//!
//! A save writes a temporary file in the directory, syncs it, renames it over the record and
//! syncs the directory, so the record is the old one or the new one and never a part of
//! either. A record whose schema is newer than [`SCHEMA`] is read-only: it is not loaded, and
//! it is not overwritten.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use js_contain::oci::lock::open_lock_file;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

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
    /// The link now points at the staged release.
    pub link_moved: bool,
    /// The unit was started on the staged release.
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

/// `path` as the check sees it, without following a link.
///
/// # Errors
/// The path cannot be looked at.
pub fn system_stat(path: &Path) -> io::Result<DirStat> {
    let meta = fs::symlink_metadata(path)?;
    Ok(DirStat {
        is_symlink: meta.file_type().is_symlink(),
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

/// The state directory, checked and locked. The lock is released when this is dropped.
///
/// It is released with an explicit unlock, not by closing the file: a lock belongs to the
/// open file description, which a child forked by another thread of this process holds a
/// copy of until it execs, so closing alone can leave the directory locked for a moment.
#[derive(Debug)]
pub struct Held {
    dir: PathBuf,
    lock: File,
}

impl Held {
    /// Checks `dir` with `stat` (see [`system_stat`]) and takes the lock, without waiting.
    ///
    /// # Errors
    /// [`StateError::Dir`] if the directory is not `root:root` 0700 and not a link,
    /// [`StateError::Lock`] if another step holds the lock, [`StateError::Io`] otherwise.
    pub fn open(
        dir: &Path,
        stat: &dyn Fn(&Path) -> io::Result<DirStat>,
    ) -> Result<Held, StateError> {
        let seen = stat(dir).map_err(at(dir))?;
        check_dir(&seen).map_err(|problem| StateError::Dir {
            path: dir.to_owned(),
            problem,
        })?;
        let path = dir.join(LOCK_FILE);
        let lock = open_lock_file(&path).map_err(at(&path))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|errno| {
            StateError::Lock {
                path,
                source: errno.into(),
            }
        })?;
        Ok(Held {
            dir: dir.to_owned(),
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
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path);
        match opened {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(at(&path)(source)),
            Ok(file) => {
                if !file.metadata().map_err(at(&path))?.is_file() {
                    return Err(StateError::Corrupt("not a regular file".to_owned()));
                }
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
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&temp)
            .map_err(at(&temp))?;
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
        fs::rename(self.dir.join(TEMP_FILE), &record).map_err(at(&record))?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(at(&self.dir))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = flock(&self.lock, FlockOperation::Unlock);
    }
}
