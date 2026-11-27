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
//! `crun` has ended, whichever is first. The pid file is looked for every [`PID_FILE_POLL`]
//! while `crun` runs, so the lock is held at most that much longer than the step. Every runtime and every agent on the node takes the
//! same file, whatever its scratch directory. The file is made by the daemon (root for a host
//! container) with mode `0600` in `/dev`, where no other user can make one or open it, so
//! no other user can hold it. A daemon that is not root takes no lock ([`node_lock`]): it
//! cannot make the file, and the `crun` it runs cannot change the host's `/dev`.
//!
//! [`Isolation::Host`]: crate::container::Isolation::Host

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use runc::Runc;
use runc::options::CreateOpts;

/// The file a node's host starts lock: one per node, in the host's `/dev`, which is what
/// `crun` changes.
pub(crate) const HOST_START_LOCK: &str = "/dev/.js-host-start.lock";

/// The pid file `crun run` writes for a host container, in its lease directory.
pub(crate) const PID_FILE: &str = "crun.pid";

/// How often a host start looks for `crun`'s pid file while it holds the lock: a `stat` of
/// the lease directory's entry, for the moment `crun` takes to start a container.
const PID_FILE_POLL: Duration = Duration::from_millis(10);

/// The lock a daemon whose effective uid is `euid` takes for its host starts:
/// [`HOST_START_LOCK`] for root, and none otherwise (a daemon that is not root can neither
/// make it nor have `crun` change the host's `/dev`).
pub(crate) fn node_lock(euid: u32) -> Option<PathBuf> {
    if euid == 0 {
        Some(PathBuf::from(HOST_START_LOCK))
    } else {
        None
    }
}

/// The pid file of the host container whose lease directory is `lease`.
fn pid_file(lease: &Path) -> PathBuf {
    lease.join(PID_FILE)
}

/// The lock held for a host start, and the pid file `crun` is to write. Dropped, the lock is
/// let go.
struct Held {
    _lock: File,
    pid_file: PathBuf,
}

/// `crun run` of the container `id` of the bundle `bundle` by `client`, with `opts`, for the
/// lease in `lease`. For a host lease ([`host_start`]) `lock` is held from before `crun`
/// starts until its pid file is there or it has ended ([`Held::across`]), and `crun` is
/// given the pid file; any other lease's run, and every run without a `lock`, takes none.
/// The result is `crun run`'s.
///
/// # Errors
/// The lock of a host lease could not be taken ([`take`]): `crun` is not started.
pub(crate) async fn run(
    client: &Runc,
    (lease, lock): (&Path, Option<&Path>),
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

/// For the lease in `lease`: `None` when there is no `lock` or it is not a host lease (no
/// [`HOST_LEASE_MARKER`](super::HOST_LEASE_MARKER) in it, a regular file), and otherwise the
/// lock `lock` held ([`take`]).
///
/// # Errors
/// The lock could not be taken ([`take`]).
async fn host_start(lease: &Path, lock: Option<&Path>) -> Result<Option<Held>, String> {
    let Some(lock) = lock else {
        return Ok(None);
    };
    let marker = lease.join(super::HOST_LEASE_MARKER).symlink_metadata();
    if !marker.is_ok_and(|meta| meta.is_file()) {
        return Ok(None);
    }
    take(lease, lock).await.map(Some)
}

/// Takes the lock `lock` (made with mode `0600` if it is not there, never through a link),
/// waiting for as long as another start holds it, for the start of the container whose lease
/// directory is `lease`. Once it is taken, a pid file left in the lease directory by an
/// earlier start is removed, before `crun` is started and can write the new one.
///
/// # Errors
/// The lock could not be opened or taken, or the old pid file could not be removed.
async fn take(lease: &Path, lock: &Path) -> Result<Held, String> {
    let lock = lock.to_owned();
    let taken = tokio::task::spawn_blocking(move || lock_file(&lock)).await;
    let lock = taken.map_err(|e| format!("the host start lock: {e}"))??;
    let pid_file = pid_file(lease);
    if pid_file.symlink_metadata().is_ok() {
        std::fs::remove_file(&pid_file).map_err(|e| format!("{}: {e}", pid_file.display()))?;
    }
    Ok(Held {
        _lock: lock,
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

impl Held {
    /// Runs `run` (`crun run`) to its end, letting the lock go as soon as the pid file is
    /// there or, if `run` ends first, when it ends. Its result is `run`'s.
    async fn across<F: Future>(self, run: F) -> F::Output {
        let mut run = pin!(run);
        let ended = {
            let mut appeared = pin!(self.appeared());
            std::future::poll_fn(|cx| match run.as_mut().poll(cx) {
                Poll::Ready(end) => Poll::Ready(Some(end)),
                Poll::Pending => appeared.as_mut().poll(cx).map(|()| None),
            })
            .await
        };
        drop(self);
        match ended {
            Some(end) => end,
            None => run.await,
        }
    }

    /// Ends once the pid file is there, looking for it every [`PID_FILE_POLL`].
    async fn appeared(&self) {
        while self.pid_file.symlink_metadata().is_err() {
            tokio::time::sleep(PID_FILE_POLL).await;
        }
    }
}

#[cfg(test)]
impl super::crun::Crun {
    /// This driver with its host starts locking `lock` (none for `None`) instead of the
    /// node's lock.
    pub(crate) fn with_start_lock(mut self, lock: Option<PathBuf>) -> Self {
        self.start_lock = lock;
        self
    }
}

#[cfg(test)]
#[path = "start_lock_tests.rs"]
mod tests;
