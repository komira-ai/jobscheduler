//! Live logs: the agent uploads the output of a `crun` job that has a sink to an S3-compatible
//! store while it runs, and writes the final files when it ends (`docs/design/live-logs.md`).
//!
//! # The layout
//!
//! A job's start may carry a sink ([`js_agent_wire::LogSink`]: the store and the job's prefix
//! `P`, no secret), checked once ([`Sink::of_start`]). Under `P` the uploader writes
//! (`js_logstore::layout` names every key):
//!
//! - **chunks** `P/stdout/<offset20>` and `P/stderr/<offset20>`: the raw bytes, 1 to 1 MiB, the
//!   offset that of the chunk's first byte in its stream, zero padded to 20 digits; the object
//!   metadata `first-byte-at-ms` is the time of the first poll that saw the chunk's first byte;
//! - a **gap marker** `P/<stream>/<offset20>.gap`, body `{"length":N}`, at the end of a stream
//!   whose last `N` bytes the capture could not keep;
//! - the **finals** `P/stdout.log` and `P/stderr.log`: each whole stream, one object.
//!
//! # Where the bytes come from
//!
//! js-contain's capture writes the first `file_cap` bytes of each stream to a file in the
//! lease directory (`--log-file-cap` for a run with a sink, the capture's 1 MiB otherwise). The
//! uploader tails those two files by byte offset; it never reads the pipes, and the file is
//! the only local backlog: it keeps no copy of its own and at most one chunk in memory (and a
//! final, at the end). It opens both files once the run has started and holds them until it
//! ends, so the removal of the lease directory at cleanup cuts nothing short.
//!
//! # Polls and cuts
//!
//! Every [`LOG_POLL`] the uploader reads each file's length; growth is noted with the poll's
//! time. A chunk is cut ([`chunker::cut`]) once [`MAX_CHUNK`] bytes wait or the oldest waiting
//! byte was seen [`LOG_FLUSH`] ago, whichever comes first, and never empty or longer than
//! `MAX_CHUNK`. A cut is kept until its put succeeded: a retry puts the same offset, length and
//! bytes (the file only grows), whatever arrived meanwhile. Within a stream chunks go out one
//! at a time in offset order. The two streams take turns: after a request of one, the other
//! goes first, so a backlog on one does not hold the other back.
//!
//! # Retries
//!
//! A failed request (the store unreachable or refusing, or the file unreadable) is tried
//! again after a backoff of one second doubled for each failure in a row, at most a minute,
//! less a spread of up to three sixteenths drawn from the job's prefix; the backoff is the
//! stream's own, and the other stream goes on meanwhile. While it fails the
//! capture keeps filling the file up to its cap; when the store answers again the stream goes
//! out in order. The credential is the host's ([`HostGrant`]), read before every request:
//! with none held no request is made, and the upload waits as for a store that is down. An
//! answer that the credential has expired marks it stale for the next heartbeat, and the next
//! request of either stream waits for the backoff or for a newer credential, whichever comes
//! first.
//!
//! # The end
//!
//! When the runtime reads the run's end (the drain is over, the totals are final) it calls
//! [`RunLogs::end`], which leaves a message and returns at once; the result reports each
//! stream's total and the bytes the capture dropped (the total less the file's length). Then,
//! per stream and on the uploader's thread: the rest of the file as chunks; a gap marker at the
//! file's length for the bytes dropped, when there are any; and, for a stream that is whole
//! (nothing dropped), not empty and at most [`Limits::max_put`] bytes, the final, read through
//! the held handle, after which every chunk put is deleted (each delete tried
//! [`DELETE_TRIES`] times, then left: the final is authoritative). A stream with a gap or over
//! the limit keeps its chunks and marker and gets no final. The work is given up
//! [`FINISH_BUDGET`] after the end (checked before every request), or at once when the
//! credential is answered as expired and no newer one has come since.
//!
//! A run removed before its end was read (a removal that ends a running container, or a start
//! that failed after the uploader started) drops the uploader without an end: its thread
//! stops at its next step and closes the files.
//!
//! # What this task leaves out
//!
//! An agent that restarts while a job runs adopts the job without an uploader: what the
//! files hold is not lost, but nothing more of that job is uploaded and its finals are not
//! written (the chunks put stay). A panic in an uploader ends that job's upload with one
//! warning and touches nothing else.

mod chunker;
mod grant;
mod sink;
mod stream;
mod upload;
mod uploader;
mod wake;

#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::LogReport;
use js_logstore::{Clock, MAX_CHUNK_BYTES, MAX_PUT_BYTES, SystemClock, Transport, UreqTransport};

pub use chunker::{CutLimits, cut};
pub use grant::{GRANT_STALE_MARGIN, HostGrant};
pub use sink::Sink;
pub use uploader::Uploader;
pub use wake::{ClockWake, ManualWake, Settled, Wake};

/// How often the uploader reads the capture files' lengths (250 ms).
pub const LOG_POLL: Duration = Duration::from_millis(250);

/// How long a waiting byte waits for its chunk (2 s).
pub const LOG_FLUSH: Duration = Duration::from_secs(2);

/// The most bytes in one chunk (1 MiB).
pub const MAX_CHUNK: usize = MAX_CHUNK_BYTES;

/// How long the end's uploads may take, from the job's end (5 minutes).
pub const FINISH_BUDGET: Duration = Duration::from_secs(300);

/// The first wait after a failed request (1 s), doubled for each failure in a row.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// The longest wait after failed requests (60 s).
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How many times one chunk's delete is tried before it is left.
pub const DELETE_TRIES: u32 = 3;

/// How long one store request may take, connect and TLS included.
pub const STORE_CALL_LIMIT: Duration = Duration::from_secs(30);

/// The default `--log-file-cap`: the capture file of each stream of a run with a sink.
pub const DEFAULT_FILE_CAP: u64 = 64 * 1024 * 1024;

/// The least `--log-file-cap`.
pub const MIN_FILE_CAP: u64 = 1024 * 1024;

/// The most `--log-file-cap`.
pub const MAX_FILE_CAP: u64 = 1024 * 1024 * 1024;

/// The limits an uploader is built with: production passes the constants of this module
/// ([`Limits::default`]), a test small ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// [`LOG_POLL`].
    pub poll: Duration,
    /// [`LOG_FLUSH`].
    pub flush: Duration,
    /// [`MAX_CHUNK`].
    pub chunk: usize,
    /// The longest final, written in one put (the store client's `MAX_PUT_BYTES`).
    pub max_put: usize,
    /// [`FINISH_BUDGET`].
    pub budget: Duration,
    /// [`BACKOFF_FIRST`].
    pub backoff_first: Duration,
    /// [`BACKOFF_MAX`].
    pub backoff_max: Duration,
    /// [`DELETE_TRIES`].
    pub delete_tries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            poll: LOG_POLL,
            flush: LOG_FLUSH,
            chunk: MAX_CHUNK,
            max_put: MAX_PUT_BYTES,
            budget: FINISH_BUDGET,
            backoff_first: BACKOFF_FIRST,
            backoff_max: BACKOFF_MAX,
            delete_tries: DELETE_TRIES,
        }
    }
}

impl Limits {
    /// What [`cut`] is given.
    #[must_use]
    pub fn cut(&self) -> CutLimits {
        CutLimits {
            chunk: self.chunk,
            flush: self.flush,
        }
    }

    /// [`Limits::max_put`] as a length.
    #[must_use]
    pub fn max_put(&self) -> u64 {
        u64::try_from(self.max_put).unwrap_or(u64::MAX)
    }
}

/// Makes the wake of each new uploader.
pub type Wakes = Arc<dyn Fn() -> Arc<dyn Wake> + Send + Sync>;

/// What a runtime that uploads live logs is given: the capture file cap of a run with a sink,
/// the host's credential, and the transport, clock, wake and limits every uploader shares.
#[derive(Clone)]
pub struct LiveLogs {
    /// The capture file cap of each stream of a run with a sink (`--log-file-cap`).
    pub file_cap: u64,
    /// The host's credential.
    pub grant: Arc<HostGrant>,
    /// The transport of the store requests.
    pub transport: Arc<dyn Transport>,
    /// The uploaders' clock.
    pub clock: Arc<dyn Clock>,
    /// Makes each uploader's wake.
    pub wakes: Wakes,
    /// The limits.
    pub limits: Limits,
}

impl fmt::Debug for LiveLogs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveLogs")
            .field("file_cap", &self.file_cap)
            .field("grant", &self.grant)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl LiveLogs {
    /// The production settings: the system clock, a transport whose requests take at most
    /// [`STORE_CALL_LIMIT`], waits on the clock and the limits of this module.
    #[must_use]
    pub fn system(file_cap: u64, grant: Arc<HostGrant>) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let waking = Arc::clone(&clock);
        Self {
            file_cap,
            grant,
            transport: Arc::new(UreqTransport::new(STORE_CALL_LIMIT)),
            clock,
            wakes: Arc::new(move || Arc::new(ClockWake::new(Arc::clone(&waking))) as Arc<dyn Wake>),
            limits: Limits::default(),
        }
    }
}

/// What the crun agent's flags say of live logs: the capture file cap of a run with a sink
/// (`--log-file-cap`) and the cell of the host's credential, which the session fills from the
/// heartbeat replies and the runtime's uploaders read.
#[derive(Debug, Clone)]
pub struct LogPlan {
    /// The capture file cap of each stream of a run with a sink.
    pub file_cap: u64,
    /// The host's credential.
    pub grant: Arc<HostGrant>,
}

impl LogPlan {
    /// A plan with the cap `file_cap` and an empty credential cell on the system clock.
    #[must_use]
    pub fn new(file_cap: u64) -> Self {
        Self {
            file_cap,
            grant: Arc::new(HostGrant::new(Arc::new(SystemClock))),
        }
    }

    /// The runtime's settings of this plan.
    #[must_use]
    pub fn live(&self) -> LiveLogs {
        LiveLogs::system(self.file_cap, Arc::clone(&self.grant))
    }
}

/// Two plans are the same when their caps are and they share one credential cell.
impl PartialEq for LogPlan {
    fn eq(&self, other: &Self) -> bool {
        self.file_cap == other.file_cap && Arc::ptr_eq(&self.grant, &other.grant)
    }
}

impl Eq for LogPlan {}

/// What a run of the `crun` runtime holds of live logs: its sink while it is staged, its
/// uploader once it has started, and the report once it has ended. A run of a runtime without
/// [`LiveLogs`], or one whose start carried no sink, holds nothing and reports nothing.
#[derive(Debug, Default)]
pub struct RunLogs {
    sink: Option<Sink>,
    uploader: Option<Uploader>,
    report: Option<LogReport>,
}

impl RunLogs {
    /// The live logs of a run whose start carried `sink`, on a runtime with `logs`.
    #[must_use]
    pub fn asked(logs: Option<&LiveLogs>, sink: Option<&Sink>) -> Self {
        Self {
            sink: logs.and(sink).cloned(),
            uploader: None,
            report: None,
        }
    }

    /// The capture file cap of the run: `logs`'s for a run with a sink, `None` (the
    /// runtime's default) otherwise.
    #[must_use]
    pub fn file_cap(&self, logs: Option<&LiveLogs>) -> Option<u64> {
        logs.filter(|_| self.sink.is_some())
            .map(|logs| logs.file_cap)
    }

    /// Starts the uploader of a run that has started, over its capture files `stdout` and
    /// `stderr`. A run without a sink starts none; one that cannot be started is logged, and the
    /// run goes on without live logs.
    pub fn start(&mut self, logs: Option<&LiveLogs>, (stdout, stderr): (&Path, &Path)) {
        self.uploader = logs
            .zip(self.sink.take())
            .and_then(|(logs, sink)| Uploader::start_or_warn(&sink, (stdout, stderr), logs));
    }

    /// The run has ended with `totals` (stdout, stderr) bytes printed: the uploader is told
    /// and the report kept. Nothing waits.
    pub fn end(&mut self, totals: (u64, u64)) {
        self.report = self
            .uploader
            .as_ref()
            .map(|uploader| uploader.finish(totals));
    }

    /// What the result reports of the run's streams, once it has ended with an uploader.
    #[must_use]
    pub fn report(&self) -> Option<LogReport> {
        self.report
    }
}
