//! A job's output: bounded on disk, with a tail in memory.
//!
//! `crun run` is given a pipe for the container's standard output and one for its
//! standard error (and no file: a job that prints without end must not fill the disk).
//! The agent keeps reading both pipes until they close, for as long as the container
//! lives, and from every chunk it keeps
//!
//! - a **file** per stream in the lease directory ([`stdout_path`], [`stderr_path`]),
//!   holding the first [`LogLimits::file_cap`] bytes of the stream. Bytes past the cap are
//!   read and discarded, never queued: the reader does not stop reading, so a job that
//!   writes ten times the cap is not blocked on a full pipe, and the memory held is one
//!   chunk;
//! - a **ring** per stream holding the last [`LogLimits::tail`] bytes, which
//!   [`Running::tail`](crate::running::Running::tail) returns as a [`LogTail`] (a value,
//!   copied out).
//!
//! The defaults match the podman path, whose log is bounded at 1 MiB a container: a cap of
//! 1 MiB a stream, and a ring of 64 KiB, well above the 8 KiB a run's result carries, so
//! that whoever cuts the tail at a character boundary has bytes to spare.
//!
//! **A restart of the agent** keeps the files (the lease directory is not touched) and
//! loses the rings, which exist only in the process that read the pipes. A container that
//! outlives its agent finds the read ends of its pipes closed: its next write is an
//! `EPIPE` (`SIGPIPE`), where a file would have taken it.
//!
//! **The end of the drain:** a stream ends when every write end of its pipe is closed,
//! which is the container's exit (the agent closes its own copies when `crun` has been
//! started and when it returns). A process the job left behind in the background holds
//! the pipe open past that exit, so the wait for the end is bounded:
//! [`LogLimits::drain_grace`] after `crun run` returned the reader is given up on, which
//! closes the read end (the straggler's next write fails) and leaves the tail as read so
//! far. The run is not over, as far as [`Running::wait`](crate::running::Running::wait)
//! is concerned, until the drain is, so the tail is complete once it returned (up to a
//! straggler's later output).
//!
//! The container's standard input is not touched: it stays the init's report file.

use std::collections::VecDeque;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::net::unix::pipe;
use tokio::task::JoinHandle;

use super::RunEnd;

/// The default for [`LogLimits::file_cap`]: 1 MiB, the podman path's bound.
pub const DEFAULT_FILE_CAP: u64 = 1024 * 1024;

/// The default for [`LogLimits::tail`]: 64 KiB.
pub const DEFAULT_TAIL: usize = 64 * 1024;

/// The default for [`LogLimits::drain_grace`].
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// How much of a job's output is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogLimits {
    /// The most bytes of each stream written to its file; the rest is discarded.
    pub file_cap: u64,
    /// The most bytes of each stream kept in memory: the last ones.
    pub tail: usize,
    /// How long after `crun run` returned the drain of a pipe that is still open (a
    /// process the job left behind holds it) is waited for.
    pub drain_grace: Duration,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            file_cap: DEFAULT_FILE_CAP,
            tail: DEFAULT_TAIL,
            drain_grace: DEFAULT_DRAIN_GRACE,
        }
    }
}

/// Where the file of the container's standard output is, in the lease directory `lease`.
pub fn stdout_path(lease: &Path) -> PathBuf {
    lease.join("stdout")
}

/// Where the file of the container's standard error is, in the lease directory `lease`.
pub fn stderr_path(lease: &Path) -> PathBuf {
    lease.join("stderr")
}

/// What one stream of a job wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamTail {
    /// The last bytes: at most [`LogLimits::tail`] of them, as written (not necessarily
    /// UTF-8, and cut anywhere).
    pub bytes: Vec<u8>,
    /// How many bytes the job wrote in all.
    pub total: u64,
    /// Whether the stream's file lacks bytes the job wrote: past the cap, or after the
    /// file could not be written.
    pub file_cut: bool,
}

/// What a job wrote to its two streams, apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogTail {
    /// The standard output.
    pub stdout: StreamTail,
    /// The standard error.
    pub stderr: StreamTail,
}

/// One stream: its file and its ring.
#[derive(Debug)]
struct Sink {
    limits: LogLimits,
    state: Mutex<SinkState>,
}

#[derive(Debug)]
struct SinkState {
    /// `None` once it could not be written.
    file: Option<std::fs::File>,
    written: u64,
    ring: VecDeque<u8>,
    total: u64,
    cut: bool,
}

impl Sink {
    fn new(file: std::fs::File, limits: LogLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(SinkState {
                file: Some(file),
                written: 0,
                ring: VecDeque::new(),
                total: 0,
                cut: false,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, SinkState> {
        // A panic while holding the lock leaves plain counters; they are still readable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes in what the job wrote next: into the ring, and into the file up to the cap.
    fn take(&self, chunk: &[u8]) {
        let mut state = self.state();
        state.total += chunk.len() as u64;
        let keep = self.limits.tail;
        // Only the last `keep` bytes of a chunk can be in the ring.
        let last = &chunk[chunk.len().saturating_sub(keep)..];
        let excess = (state.ring.len() + last.len()).saturating_sub(keep);
        state.ring.drain(..excess);
        state.ring.extend(last);

        let room = self.limits.file_cap.saturating_sub(state.written);
        let fits = usize::try_from(room).map_or(chunk.len(), |room| room.min(chunk.len()));
        if fits < chunk.len() {
            state.cut = true;
        }
        if let Some(mut file) = state.file.take() {
            use std::io::Write as _;
            // A file that cannot be written (the disk is full) is given up on; the
            // reading goes on.
            if file.write_all(&chunk[..fits]).is_ok() {
                state.written += fits as u64;
                state.file = Some(file);
            } else {
                state.cut = true;
            }
        }
    }

    fn tail(&self) -> StreamTail {
        let state = self.state();
        StreamTail {
            bytes: state.ring.iter().copied().collect(),
            total: state.total,
            file_cut: state.cut,
        }
    }
}

/// The two streams of one run, read by [`supervise`] and read back with [`Capture::tail`].
#[derive(Debug)]
pub(crate) struct Capture {
    out: Arc<Sink>,
    err: Arc<Sink>,
    drain_grace: Duration,
}

impl Capture {
    /// The (new, empty) files of the lease in `lease`.
    pub(crate) fn create(lease: &Path, limits: LogLimits) -> Result<Self, String> {
        let make = |path: PathBuf| {
            std::fs::File::create(&path)
                .map(|file| Arc::new(Sink::new(file, limits)))
                .map_err(|e| format!("{}: {e}", path.display()))
        };
        Ok(Self {
            out: make(stdout_path(lease))?,
            err: make(stderr_path(lease))?,
            drain_grace: limits.drain_grace,
        })
    }

    /// A copy of what has been read so far.
    pub(crate) fn tail(&self) -> LogTail {
        LogTail {
            stdout: self.out.tail(),
            stderr: self.err.tail(),
        }
    }
}

/// What a read of a pipe takes at most.
const CHUNK: usize = 8 * 1024;

/// Reads `pipe` until it closes (or fails) and gives everything to `sink`.
async fn pump(mut pipe: pipe::Receiver, sink: Arc<Sink>) {
    let mut chunk = vec![0_u8; CHUNK];
    while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
        sink.take(&chunk[..n]);
    }
}

/// The two readers of one run.
struct Drain {
    out: JoinHandle<()>,
    err: JoinHandle<()>,
}

impl Drain {
    /// Whether both pipes closed within `grace`; if not, the readers are stopped.
    async fn finish(mut self, grace: Duration) -> bool {
        let both = async {
            // A reader that panicked has nothing more to say.
            let _ = (&mut self.out).await;
            let _ = (&mut self.err).await;
        };
        let closed = tokio::time::timeout(grace, both).await.is_ok();
        self.out.abort();
        self.err.abort();
        closed
    }
}

type Pipe = fn() -> std::io::Result<(pipe::Sender, pipe::Receiver)>;

/// How a `crun run` is started once its pipes are made: given the write ends of the
/// container's standard output and standard error.
pub(crate) type Launch =
    Box<dyn FnOnce(OwnedFd, OwnedFd) -> Pin<Box<dyn Future<Output = RunEnd> + Send>> + Send>;

/// Runs `launch` with new pipes, drains them into `capture` meanwhile, and once it has
/// returned waits for the drain as long as the capture's limits say.
pub(crate) async fn supervise(capture: Arc<Capture>, launch: Launch) -> RunEnd {
    supervise_with(pipe::pipe, capture, launch).await
}

/// The two pipes of a run: the write ends, blocking, for the child, and the readers.
type Pipes = (OwnedFd, pipe::Receiver, OwnedFd, pipe::Receiver);

fn open_pipes(make: Pipe) -> std::io::Result<Pipes> {
    let (out_tx, out_rx) = make()?;
    let (err_tx, err_rx) = make()?;
    Ok((
        out_tx.into_blocking_fd()?,
        out_rx,
        err_tx.into_blocking_fd()?,
        err_rx,
    ))
}

/// [`supervise`] with `make` making the pipes: the one place the pipes can fail, a
/// test's to cause.
async fn supervise_with(make: Pipe, capture: Arc<Capture>, launch: Launch) -> RunEnd {
    let (out, out_rx, err, err_rx) = match open_pipes(make) {
        Ok(pipes) => pipes,
        Err(e) => return RunEnd::Failed(format!("the pipes of the output: {e}")),
    };
    let drain = Drain {
        out: tokio::spawn(pump(out_rx, capture.out.clone())),
        err: tokio::spawn(pump(err_rx, capture.err.clone())),
    };
    // `launch` owns the write ends; they are closed when it returns.
    let end = launch(out, err).await;
    if !drain.finish(capture.drain_grace).await {
        tracing::warn!("a process of the job holds its output open after the container ended");
    }
    end
}

#[cfg(test)]
#[path = "logs_tests.rs"]
mod tests;
