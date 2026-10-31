//! The lock of a scratch directory: any number of OCI backends (agents) share a scratch
//! directory, and a `js-agent image-pull` has it alone.
//!
//! Starting a backend sweeps the image stages an earlier run left behind
//! ([`super::image::Images::sweep`]), which would include the stage of an unpack under way in
//! another process. The protocol on [`LOCK_FILE`] (`flock`, one open file description per
//! holder):
//! - A backend that opens as an agent ([`Use::Shared`]) first tries `LOCK_EX|LOCK_NB`. If it
//!   gets it, no other holder is live: it sweeps, then converts the lock to `LOCK_SH`
//!   ([`ScratchLock::share`]) and keeps it for as long as it lives. If it does not, it skips
//!   the stage sweep (another holder's stage may be in flight) and takes `LOCK_SH`, waiting
//!   for it.
//! - A pull ([`Use::Exclusive`]) takes `LOCK_EX|LOCK_NB` and, if it does not get it, is
//!   refused ([`OciError::Locked`](super::OciError::Locked)) before it changes anything.
//!
//! The wait for `LOCK_SH` cannot be a deadlock: nobody holding the lock waits for anything
//! while holding it except the converting sweeper, which waits for nobody (it holds the
//! lock exclusively, so no other holder exists and its conversion to shared cannot be
//! blocked). `flock` conversion is not atomic, so another opener's `LOCK_SH` in the sweeper's
//! window is not refused but waits for the sweep to finish. The one longer wait is an agent
//! that starts while a pull holds the lock: it waits for the pull to end.

use std::fs::File;
use std::path::Path;

use rustix::fs::{FlockOperation as Op, Mode, OFlags, flock};

use super::{OciError, at, prepare};

/// The lock file, directly in the scratch directory.
pub const LOCK_FILE: &str = ".js-oci.lock";

/// How a backend uses the scratch directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// Beside other agents: the stage sweep runs only when this is the only holder.
    Shared,
    /// Alone, as a pull is: refused if anything else holds the directory.
    Exclusive,
}

/// The lock, released when this is dropped.
///
/// It is released with an explicit unlock, not by closing the file: a lock belongs to the
/// open file description, which a child forked by another thread of this process holds a
/// copy of until it execs, so closing alone can leave the directory locked for a moment.
#[derive(Debug)]
pub(crate) struct ScratchLock {
    file: File,
    /// Whether the stage sweep may run: no other holder is live.
    alone: bool,
    /// Whether the lock is to be converted to shared once the sweep is done.
    converts: bool,
}

impl ScratchLock {
    /// Takes the lock of `scratch` for `how`.
    ///
    /// # Errors
    /// The lock file cannot be opened, or an exclusive lock is wanted and another holder
    /// has the lock.
    pub(crate) fn take(scratch: &Path, how: Use) -> Result<Self, OciError> {
        let path = scratch.join(LOCK_FILE);
        // Never through a link.
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = rustix::fs::open(&path, flags, Mode::RUSR | Mode::WUSR)
            .map(File::from)
            .map_err(std::io::Error::from);
        let file = at(&path, opened).map_err(prepare)?;
        let exclusive = flock(&file, Op::NonBlockingLockExclusive);
        let alone = exclusive.is_ok();
        let taken = if how == Use::Shared && !alone {
            flock(&file, Op::LockShared)
        } else {
            exclusive
        };
        held(&path, taken)?;
        Ok(Self {
            file,
            alone,
            converts: alone && how == Use::Shared,
        })
    }

    /// Whether this holder has the directory to itself: it may sweep the stages.
    pub(crate) fn alone(&self) -> bool {
        self.alone
    }

    /// Lets other agents in: converts an exclusive lock to a shared one. A no-op for a
    /// lock that was taken shared, or exclusive for good.
    ///
    /// # Errors
    /// The conversion failed (the lock is then still held as it was).
    pub(crate) fn share(&self) -> Result<(), OciError> {
        if self.converts {
            held(Path::new(LOCK_FILE), flock(&self.file, Op::LockShared))?;
        }
        Ok(())
    }
}

/// The outcome of a `flock` on the lock file `path`, as the error says it.
fn held(path: &Path, got: Result<(), rustix::io::Errno>) -> Result<(), OciError> {
    got.map_err(|why| OciError::Locked {
        path: path.to_owned(),
        why: why.to_string(),
    })
}

impl Drop for ScratchLock {
    fn drop(&mut self) {
        let _ = flock(&self.file, Op::Unlock);
    }
}
