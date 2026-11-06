//! The uploader of one job: a thread of its own that drives the job's [`Upload`], and the
//! handle the runtime keeps.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use js_agent_wire::LogReport;

use super::LiveLogs;
use super::record::{Recorder, Streams};
use super::sink::Sink;
use super::stream::End;
use super::upload::{Ends, Message, Next, Upload};
use super::wake::Wake;

/// The handle of one job's uploader. The thread holds the two capture files open from the
/// start, so the removal of the lease directory does not cut its work short.
///
/// Nothing here waits for the thread: [`Uploader::finish`] leaves a message and wakes it.
/// Dropping the handle after `finish` leaves the thread to end by itself (at the latest
/// [`Limits::budget`](super::Limits::budget) after the end); dropping it before (the run was
/// removed before its end was read) tells the thread to stop at its next step, which closes
/// the files.
pub struct Uploader {
    files: [Arc<File>; 2],
    /// The most bytes of each file the upload reads.
    cap: u64,
    recorder: Recorder,
    mailbox: Arc<Mutex<Option<Message>>>,
    wake: Arc<dyn Wake>,
    thread: Option<JoinHandle<()>>,
    finished: AtomicBool,
}

impl std::fmt::Debug for Uploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uploader").finish_non_exhaustive()
    }
}

/// Where an uploader begins: the directory it keeps its progress record in (none keeps no
/// record), where each stream's upload is (the start of both for a job that starts, what
/// the record said for one adopted after a restart), and how much of each file it reads.
#[derive(Debug, Default)]
pub(super) struct Origin {
    /// The lease directory, where the record is kept.
    pub(super) record: Option<PathBuf>,
    /// Where each stream's upload is.
    pub(super) streams: Streams,
    /// The most bytes of each file the upload reads; `None` reads all of it. A capture that
    /// stops at its cap (`crun`'s) needs none; the files of a runtime that has no such capture
    /// (the native runtime's, which the job writes itself) are read up to the run's file cap,
    /// and what lies past it counts as dropped.
    pub(super) cap: Option<u64>,
}

impl Uploader {
    /// Opens `stdout` and `stderr` (the capture files of a run that has started) and starts the
    /// thread that uploads them to `sink`, from the start of both and keeping no progress record.
    ///
    /// # Errors
    ///
    /// A file could not be opened.
    pub fn start(
        sink: &Sink,
        (stdout, stderr): (&Path, &Path),
        logs: &LiveLogs,
    ) -> io::Result<Self> {
        Self::open(sink, (stdout, stderr), logs, Origin::default())
    }

    /// The one constructor of every uploader, at a job's start and at its adoption after a
    /// restart: opens `stdout` and `stderr` and starts the thread that uploads them to `sink`
    /// from `origin`. The thread writes the progress record (when `origin` names its directory)
    /// before its first step.
    ///
    /// # Errors
    ///
    /// A file could not be opened.
    pub(super) fn open(
        sink: &Sink,
        (stdout, stderr): (&Path, &Path),
        logs: &LiveLogs,
        origin: Origin,
    ) -> io::Result<Self> {
        let files = [Arc::new(capture(stdout)?), Arc::new(capture(stderr)?)];
        let mailbox = Arc::new(Mutex::new(None));
        let wake = (logs.wakes)();
        let recorder = Recorder::new(origin.record, sink);
        let cap = origin.cap.unwrap_or(u64::MAX);
        let mut upload = Upload::new(
            sink.clone(),
            (files.clone(), cap),
            (
                Arc::clone(&logs.transport),
                Arc::clone(&logs.clock),
                Arc::clone(&logs.grant),
            ),
            logs.limits,
            (recorder.clone(), origin.streams),
        );
        let (inbox, sleeper) = (Arc::clone(&mailbox), Arc::clone(&wake));
        let prefix = sink.prefix().as_str().to_owned();
        // The thread logs where its starter does.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let ran = catch_unwind(AssertUnwindSafe(|| {
                    upload.begin();
                    while let Next::Sleep(until) = upload.step(take(&inbox)) {
                        sleeper.sleep_until(until);
                    }
                }));
                if ran.is_err() {
                    warn(
                        &prefix,
                        "the uploader stopped on a panic; this job's upload ends here",
                    );
                }
                sleeper.ended();
            });
        });
        Ok(Self {
            files,
            cap,
            recorder,
            mailbox,
            wake,
            thread: Some(thread),
            finished: AtomicBool::new(false),
        })
    }

    /// [`Uploader::open`], or `None` with one warning naming the job's prefix and why.
    pub(super) fn open_or_warn(
        sink: &Sink,
        (stdout, stderr): (&Path, &Path),
        logs: &LiveLogs,
        origin: Origin,
    ) -> Option<Self> {
        Self::open(sink, (stdout, stderr), logs, origin)
            .map_err(|why| {
                warn(
                    sink.prefix().as_str(),
                    &format!("the uploader did not start: {why}"),
                )
            })
            .ok()
    }

    /// Tells the thread that the job has ended with `totals` (stdout, stderr) bytes printed,
    /// as the capture counted them once its drain was over, and returns what the result
    /// reports: each stream's total, and the bytes the capture could not keep (the total less
    /// the file's length, the length counted up to the cap). It does not wait for the thread.
    pub fn finish(&self, (stdout, stderr): (u64, u64)) -> LogReport {
        let [out, err] =
            [(&self.files[0], stdout), (&self.files[1], stderr)].map(|(file, total)| {
                let length = file.metadata().map_or(0, |meta| meta.len().min(self.cap));
                End {
                    length,
                    dropped: total.saturating_sub(length),
                }
            });
        self.finished.store(true, Ordering::SeqCst);
        self.post(Message::End(Ends([out, err])));
        LogReport {
            stdout_bytes: stdout,
            stderr_bytes: stderr,
            stdout_dropped: out.dropped,
            stderr_dropped: err.dropped,
        }
    }

    /// Writes no progress record from now on and removes the one written, before the lease
    /// directory that holds it is removed; a write under way has ended when this returns. The
    /// upload itself goes on.
    pub(super) fn leave(&self) {
        self.recorder.leave();
    }

    /// Waits for the thread to end, for a caller that must know it has (a test). Whether it
    /// ended without a panic that escaped (the thread catches its own).
    pub fn join(mut self) -> bool {
        self.thread
            .take()
            .is_some_and(|thread| thread.join().is_ok())
    }

    /// Leaves `message` for the thread and wakes it.
    fn post(&self, message: Message) {
        *self.mailbox.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
        self.wake.notify();
    }
}

/// A handle dropped before [`Uploader::finish`] stops the thread.
impl Drop for Uploader {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            self.post(Message::Dropped);
        }
    }
}

/// Logs `what` as a warning about the live logs of the job whose prefix is `prefix`.
fn warn(prefix: &str, what: &str) {
    tracing::warn!(prefix, "live logs: {what}");
}

/// Opens the capture file `path` to read it. The lease directory is the job user's, who may put
/// something else in the file's place while no agent runs: a link is refused, not followed, a
/// pipe is opened without waiting for a writer, and anything but a regular file is refused.
fn capture(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    match file.metadata()?.is_file() {
        true => Ok(file),
        false => Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        ))),
    }
}

/// The message the mailbox holds, taken.
fn take(mailbox: &Mutex<Option<Message>>) -> Option<Message> {
    mailbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}
