//! The lock of a scratch directory: one OCI backend, or one `js-agent image-pull`, uses a
//! scratch directory at a time.
//!
//! Starting the backend sweeps the image stages an earlier run left behind
//! ([`super::image::Images::sweep`]), which would include the stage of a pull that is under
//! way in another process. The backend therefore holds an exclusive lock on
//! [`LOCK_FILE`] for as long as it lives, and a second start on the same directory is
//! refused ([`OciError::Locked`](super::OciError::Locked)) before it changes anything.

use std::fs::File;
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use super::{OciError, at, prepare};

/// The lock file, directly in the scratch directory.
pub const LOCK_FILE: &str = ".js-oci.lock";

/// The lock, released when this is dropped.
///
/// It is released with an explicit unlock, not by closing the file: a lock belongs to the
/// open file description, which a child forked by another thread of this process holds a
/// copy of until it execs, so closing alone can leave the directory locked for a moment.
#[derive(Debug)]
pub(crate) struct ScratchLock(File);

impl ScratchLock {
    /// Takes the lock of `scratch`, without waiting.
    ///
    /// # Errors
    /// The lock file cannot be opened, or another holder has the lock.
    pub(crate) fn take(scratch: &Path) -> Result<Self, OciError> {
        let path = scratch.join(LOCK_FILE);
        // Never through a link.
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = rustix::fs::open(&path, flags, Mode::RUSR | Mode::WUSR)
            .map(File::from)
            .map_err(std::io::Error::from);
        let file = at(&path, opened).map_err(prepare)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |why| OciError::Locked {
                path,
                why: why.to_string(),
            },
        )?;
        Ok(Self(file))
    }
}

impl Drop for ScratchLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}
