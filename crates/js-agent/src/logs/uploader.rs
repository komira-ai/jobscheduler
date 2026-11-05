//! The uploader of one job: a thread of its own that drives the job's [`Upload`], and the
//! handle the runtime keeps.

use std::fs::File;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use js_agent_wire::LogReport;

use super::LiveLogs;
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

impl Uploader {
    /// Opens `stdout` and `stderr` (the capture files of a run that has started) and starts the
    /// thread that uploads them to `sink`.
    ///
    /// # Errors
    ///
    /// A file could not be opened, or the thread could not be started.
    pub fn start(
        sink: &Sink,
        (stdout, stderr): (&Path, &Path),
        logs: &LiveLogs,
    ) -> io::Result<Self> {
        let files = [Arc::new(File::open(stdout)?), Arc::new(File::open(stderr)?)];
        let mailbox = Arc::new(Mutex::new(None));
        let wake = (logs.wakes)();
        let mut upload = Upload::new(
            sink.clone(),
            files.clone(),
            (
                Arc::clone(&logs.transport),
                Arc::clone(&logs.clock),
                Arc::clone(&logs.grant),
            ),
            logs.limits,
        );
        let (inbox, sleeper) = (Arc::clone(&mailbox), Arc::clone(&wake));
        let prefix = sink.prefix().as_str().to_owned();
        // The thread logs where its starter does.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread = std::thread::Builder::new()
            .name("js-agent-logs".to_owned())
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    let ran = catch_unwind(AssertUnwindSafe(|| {
                        while let Next::Sleep(until) = upload.step(take(&inbox)) {
                            sleeper.sleep_until(until);
                        }
                    }));
                    if ran.is_err() {
                        tracing::warn!(
                            prefix = prefix.as_str(),
                            "live logs: the uploader stopped on a panic; this job's upload ends here"
                        );
                    }
                    sleeper.ended();
                });
            })?;
        Ok(Self {
            files,
            mailbox,
            wake,
            thread: Some(thread),
            finished: AtomicBool::new(false),
        })
    }

    /// Tells the thread that the job has ended with `totals` (stdout, stderr) bytes printed,
    /// as the capture counted them once its drain was over, and returns what the result
    /// reports: each stream's total, and the bytes the capture could not keep (the total less
    /// the file's length). It does not wait for the thread.
    pub fn finish(&self, (stdout, stderr): (u64, u64)) -> LogReport {
        let [out, err] =
            [(&self.files[0], stdout), (&self.files[1], stderr)].map(|(file, total)| {
                let length = file.metadata().map_or(0, |meta| meta.len());
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

/// The message the mailbox holds, taken.
fn take(mailbox: &Mutex<Option<Message>>) -> Option<Message> {
    mailbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}
