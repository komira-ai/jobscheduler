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
//!   the stage sweep (another holder's stage may be in flight) and takes `LOCK_SH`: at once
//!   if only agents hold it, else (a pull, or a sweeper in its window) it logs that it is
//!   waiting, naming the lock file, and waits.
//! - A pull ([`Use::Exclusive`]) takes `LOCK_EX|LOCK_NB` and, if it does not get it, is
//!   refused ([`OciError::Locked`](super::OciError::Locked)) before it changes anything.
//!
//! The wait for `LOCK_SH` cannot be a deadlock: nobody holding the lock waits for anything
//! while holding it except the converting sweeper, and it waits only for a holder that waits
//! for nothing. The conversion is not atomic: Linux `flock` converts by dropping the old lock
//! and taking the new one, so in that window another opener gets in. An opener's `LOCK_SH`
//! then waits for the sweep to finish; and a pull's `LOCK_EX|LOCK_NB` can slip in, so the
//! sweeper's own `LOCK_SH` waits for that pull to end (safe: its sweep is done and it
//! touches nothing meanwhile). The one longer wait is an agent that starts while a pull holds
//! the lock: it waits for the pull to end.
//!
//! The lock file is opened without following a link and without blocking (a FIFO put in its
//! place cannot hang the start), and must be a regular file owned by the running user.

use std::fs::File;
use std::path::{Path, PathBuf};

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
    /// The lock file, for the words of an error.
    path: PathBuf,
    /// Whether the stage sweep may run: no other holder is live.
    alone: bool,
    /// Whether the lock is to be converted to shared once the sweep is done.
    converts: bool,
}

/// Opens the lock file `path`, making it if it is not there: without following a link and
/// without blocking (a FIFO in its place is refused, not waited on), and it must be a regular
/// file owned by the running user.
///
/// # Errors
/// The file cannot be opened or is not as required, in words that name `path`.
pub fn open_lock_file(path: &Path) -> std::io::Result<File> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let file = File::from(rustix::fs::open(
        path,
        flags | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    plain_file_of(&file, rustix::process::geteuid().as_raw())?;
    Ok(file)
}

/// Checks that `file` is a regular file owned by `uid`.
fn plain_file_of(file: &File, uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = file.metadata()?;
    let plain = meta.is_file() && meta.uid() == uid;
    let said = format!(
        "not a regular file owned by uid {uid} (owner {})",
        meta.uid()
    );
    plain
        .then_some(())
        .ok_or_else(|| std::io::Error::other(said))
}

impl ScratchLock {
    /// Takes the lock of `scratch` for `how`.
    ///
    /// # Errors
    /// The lock file cannot be opened, or an exclusive lock is wanted and another holder
    /// has the lock.
    pub(crate) fn take(scratch: &Path, how: Use) -> Result<Self, OciError> {
        Self::take_with(scratch, how, None)
    }

    /// [`ScratchLock::take`], calling `waiting` (if given) just before it waits for a lock.
    fn take_with(
        scratch: &Path,
        how: Use,
        waiting: Option<&mut dyn FnMut()>,
    ) -> Result<Self, OciError> {
        let path = scratch.join(LOCK_FILE);
        let file = at(&path, open_lock_file(&path)).map_err(prepare)?;
        let exclusive = flock(&file, Op::NonBlockingLockExclusive);
        let alone = exclusive.is_ok();
        let mut taken = exclusive;
        if how == Use::Shared && !alone {
            taken = flock(&file, Op::NonBlockingLockShared);
        }
        if how == Use::Shared && taken.is_err_and(|e| e == rustix::io::Errno::WOULDBLOCK) {
            tracing::info!(path = %path.display(), "waiting for a pull or sweep holding the lock");
            waiting.into_iter().for_each(|hook| hook());
            taken = flock(&file, Op::LockShared);
        }
        held(&path, taken)?;
        Ok(Self {
            file,
            path,
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
            held(&self.path, flock(&self.file, Op::LockShared))?;
        }
        Ok(())
    }
}

/// The outcome of a `flock` on the lock file `path`: only a lock that is held by another is
/// [`OciError::Locked`]; any other failure is reported as itself.
fn held(path: &Path, got: Result<(), rustix::io::Errno>) -> Result<(), OciError> {
    match got {
        Ok(()) => Ok(()),
        Err(why) if why == rustix::io::Errno::WOULDBLOCK => Err(OciError::Locked {
            path: path.to_owned(),
            why: why.to_string(),
        }),
        Err(other) => at(path, Err(std::io::Error::from(other))).map_err(prepare),
    }
}

impl Drop for ScratchLock {
    fn drop(&mut self) {
        let _ = flock(&self.file, Op::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("js-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// Catches a lock file that is a link (a planted link would lock, or create, its
    /// target), one that is a FIFO (opening it for writing would hang the start with no
    /// reader, and with a reader it is no lock file), and one owned by another user.
    #[test]
    fn the_lock_file_must_be_a_plain_file_of_the_running_user_and_not_a_link() {
        let dir = scratch("plain");
        let target = dir.join("target");
        std::fs::write(&target, "").expect("target");
        let link = dir.join("link");
        symlink(&target, &link).expect("link");
        assert!(open_lock_file(&link).is_err(), "followed a link");

        let fifo = dir.join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .expect("fifo");
        assert!(
            open_lock_file(&fifo).is_err(),
            "opened a FIFO with no reader"
        );
        let reader = rustix::fs::open(&fifo, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())
            .map(File::from)
            .expect("reader");
        let why = open_lock_file(&fifo).expect_err("a FIFO").to_string();
        assert!(why.contains("not a regular file"), "{why}");
        drop(reader);

        let own = open_lock_file(&dir.join("lock")).expect("a new lock file");
        assert_eq!(
            own.metadata().expect("meta").permissions().mode() & 0o777,
            0o600
        );
        let me = rustix::process::geteuid().as_raw();
        assert!(plain_file_of(&own, me).is_ok());
        let why = plain_file_of(&own, me + 1)
            .expect_err("another owner")
            .to_string();
        assert!(why.contains("not a regular file owned by uid"), "{why}");
    }

    /// Catches a lock failure that is not "held by another" reported as that (the pull would
    /// call a full lock table a running agent): only `EWOULDBLOCK` is `Locked`, any other
    /// errno is a start error that says what it was, and success is success.
    #[test]
    fn only_a_lock_held_by_another_is_locked() {
        let path = Path::new("/s/.js-oci.lock");
        assert!(held(path, Ok(())).is_ok());
        let busy = held(path, Err(rustix::io::Errno::WOULDBLOCK)).expect_err("busy");
        assert!(matches!(busy, OciError::Locked { .. }), "{busy}");
        let other = held(path, Err(rustix::io::Errno::NOLCK)).expect_err("no locks");
        assert!(!matches!(other, OciError::Locked { .. }), "{other}");
        assert!(other.to_string().contains("/s/.js-oci.lock"), "{other}");
    }

    /// Catches an agent that starts behind a pull and waits without saying so, and one that
    /// waits behind agents only: with a pull holding the lock the hook runs before the wait
    /// (here it ends the pull, so the wait is over at once); beside agents it does not run.
    #[test]
    fn an_agent_behind_a_pull_says_it_is_waiting_and_one_beside_agents_does_not() {
        let dir = scratch("waits");
        let pull = ScratchLock::take(&dir, Use::Exclusive).expect("the pull");
        let mut pull = Some(pull);
        let mut said = 0;
        let agent = ScratchLock::take_with(
            &dir,
            Use::Shared,
            Some(&mut || {
                said += 1;
                drop(pull.take());
            }),
        )
        .expect("the agent, once the pull is over");
        assert_eq!(said, 1);
        assert!(!agent.alone());
        let mut beside = 0;
        let second = ScratchLock::take_with(&dir, Use::Shared, Some(&mut || beside += 1))
            .expect("beside the agent");
        assert_eq!(beside, 0, "waited behind an agent");
        drop((agent, second));
    }
}
