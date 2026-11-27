//! The lock a host container's start ([`Isolation::Host`]) holds while `crun` is at its
//! `/dev` step, so that two host starts on a node never run that step at once.
//!
//! A host container's root is the host's `/` and it has no mount namespace, so `crun`
//! (`create_missing_devs`, in every version from 1.14) prepares the host's own `/dev` as a
//! container's: it makes the standard device nodes and links where they are missing, and
//! makes the link `ptmx -> pts/ptmx` by force (`symlinkat`; on `EEXIST`, `unlinkat` and
//! `symlinkat` again). Two host starts that reach that step together both find the entry,
//! one removes it, the other's removal fails (`ENOENT`), and that start ends with
//! `creating symlink for /dev/ptmx: File exists` before the job's program runs. A contained
//! container mounts a `/dev` of its own and is not concerned.
//!
//! A host start takes [`HOST_START_LOCK`] (an exclusive `flock`) before `crun run` is
//! started, and lets it go once `crun` has written its pid file ([`PID_FILE`], `--pid-file`),
//! which `crun run` writes after the container's process has passed its `/dev` step, or once
//! `crun` has ended, whichever is first. Every runtime and every agent on the node takes the
//! same file, whatever its scratch directory. The file is made by the daemon (root for a host
//! container) with mode `0600` in `/dev`, where no other user can make one or open it, so
//! no other user can hold it.
//!
//! [`Isolation::Host`]: crate::container::Isolation::Host

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::task::Poll;

use runc::Runc;
use runc::options::CreateOpts;
use rustix::fs::inotify;
use tokio::io::unix::AsyncFd;

/// The file a node's host starts lock: one per node, in the host's `/dev`, which is what
/// `crun` changes.
pub(crate) const HOST_START_LOCK: &str = "/dev/.js-host-start.lock";

/// The pid file `crun run` writes for a host container, in its lease directory.
pub(crate) const PID_FILE: &str = "crun.pid";

/// The pid file of the host container whose lease directory is `lease`.
fn pid_file(lease: &Path) -> PathBuf {
    lease.join(PID_FILE)
}

/// The lock held for a host start, and the watch on the lease directory that sees `crun`'s
/// pid file appear. Dropped, the lock is let go.
struct Held {
    _lock: File,
    watch: AsyncFd<OwnedFd>,
    pid_file: PathBuf,
}

/// `crun run` of the container `id` of the bundle `bundle` by `client`, with `opts`, for the
/// lease in `lease`. For a host lease ([`host_start`]) `lock` is held from before `crun`
/// starts until its pid file is there or it has ended ([`Held::across`]), and `crun` is
/// given the pid file; any other lease's run takes no lock. The result is `crun run`'s.
///
/// # Errors
/// The lock of a host lease could not be taken ([`take`]): `crun` is not started.
pub(crate) async fn run(
    client: &Runc,
    (lease, lock): (&Path, &Path),
    (id, bundle): (&str, &Path),
    opts: CreateOpts,
) -> Result<runc::Result<()>, String> {
    match host_start(lease, lock).await? {
        Some(held) => {
            let opts = opts.pid_file(pid_file(lease));
            Ok(held.across(client.run(id, bundle, Some(&opts))).await)
        }
        None => Ok(client.run(id, bundle, Some(&opts)).await),
    }
}

/// For the lease in `lease`: `None` when it is not a host lease (no
/// [`HOST_LEASE_MARKER`](super::HOST_LEASE_MARKER) in it, a regular file), and otherwise the
/// lock `lock` held ([`take`]).
///
/// # Errors
/// The lock could not be taken ([`take`]).
async fn host_start(lease: &Path, lock: &Path) -> Result<Option<Held>, String> {
    let marker = lease.join(super::HOST_LEASE_MARKER).symlink_metadata();
    if !marker.is_ok_and(|meta| meta.is_file()) {
        return Ok(None);
    }
    take(lease, lock).await.map(Some)
}

/// Takes the lock `lock` (made with mode `0600` if it is not there, never through a link),
/// waiting for as long as another start holds it, for the start of the container whose lease
/// directory is `lease`. Once it is taken (so that a start waiting for it holds nothing
/// more), a pid file left in the lease directory by an earlier start is removed and the
/// directory is watched for the new one, before `crun` is started and can write it.
///
/// # Errors
/// The lock could not be opened or taken, the old pid file could not be removed, or the watch
/// could not be made.
async fn take(lease: &Path, lock: &Path) -> Result<Held, String> {
    let lock = lock.to_owned();
    let taken = tokio::task::spawn_blocking(move || lock_file(&lock)).await;
    let lock = taken.map_err(|e| format!("the host start lock: {e}"))??;
    let pid_file = pid_file(lease);
    if pid_file.symlink_metadata().is_ok() {
        std::fs::remove_file(&pid_file).map_err(|e| format!("{}: {e}", pid_file.display()))?;
    }
    let watch = watch(lease).map_err(|e| format!("watch {}: {e}", lease.display()))?;
    Ok(Held {
        _lock: lock,
        watch,
        pid_file,
    })
}

/// Opens `path` (made with mode `0600` if it is not there, never through a link) and takes
/// an exclusive `flock` on it, waiting for whoever holds it.
fn lock_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("the host start lock {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("the host start lock {}: {e}", path.display()))?;
    Ok(file)
}

/// An inotify watch for entries made or moved into `dir`, not blocking, on the runtime.
fn watch(dir: &Path) -> std::io::Result<AsyncFd<OwnedFd>> {
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
    inotify::add_watch(
        &fd,
        dir,
        inotify::WatchFlags::CREATE | inotify::WatchFlags::MOVED_TO,
    )?;
    AsyncFd::new(fd)
}

impl Held {
    /// Runs `run` (`crun run`) to its end, letting the lock go as soon as the pid file is
    /// there or, if `run` ends first, when it ends. Its result is `run`'s.
    async fn across<F: Future>(self, run: F) -> F::Output {
        let mut run = pin!(run);
        let ended = {
            let mut appeared = pin!(self.appeared());
            std::future::poll_fn(|cx| match run.as_mut().poll(cx) {
                Poll::Ready(end) => Poll::Ready(Some(end)),
                Poll::Pending => appeared.as_mut().poll(cx).map(|_| None),
            })
            .await
        };
        drop(self);
        match ended {
            Some(end) => end,
            None => run.await,
        }
    }

    /// Ends once the pid file is there. Woken by the watch, which only says that something
    /// was made in the lease directory: what it read is discarded, and the file is looked for
    /// again. An error of the watch ends it too, so the lock is let go.
    async fn appeared(&self) -> std::io::Result<()> {
        let mut events = [0u8; 4096];
        while self.pid_file.symlink_metadata().is_err() {
            let mut ready = self.watch.readable().await?;
            while rustix::io::read(self.watch.get_ref(), &mut events).is_ok() {}
            ready.clear_ready();
        }
        Ok(())
    }
}

#[cfg(test)]
impl super::crun::Crun {
    /// This driver with its host starts locking `lock` instead of the node's lock.
    pub(crate) fn with_start_lock(mut self, lock: PathBuf) -> Self {
        self.start_lock = lock;
        self
    }
}

#[cfg(test)]
#[path = "start_lock_tests.rs"]
mod tests;
