//! A finished container's log: how its size is bounded on the host, and how the tail the
//! result carries is read.
//!
//! The engine keeps what the job wrote to its standard output and standard error in a
//! log it manages. [`log_args`] puts a size bound on that log when the container is
//! created, so a job that prints without end cannot fill the host's disk (the bound is
//! [`LOG_MAX_SIZE`] a container; Docker keeps one more file of that size, which is why a
//! tail read just after a rotation is still not empty). [`CliRuntime::read_logs`] runs
//! `logs` once on a container that has exited and keeps the last [`MAX_LOG_STREAM_LEN`]
//! bytes of each stream, apart, as a [`LogTail`].
//!
//! The tail is returned to whoever can read the run, as the job wrote it: a job must not
//! print a secret.

use std::time::Duration;

use js_agent_wire::LogTail;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;

use super::{CliError, CliRuntime, Engine, check, engine_env};
use crate::facts::Streams;
use crate::lifecycle::RunHandle;

/// The size, in bytes, at which the engine rotates a container's log. Both engines read a
/// plain number of bytes; this is 1 MiB, far above what a tail holds.
pub const LOG_MAX_SIZE: &str = "1048576";

/// How long the `logs` call may take (a runtime's own is set by
/// [`CliRuntime::with_logs_limit`]). It reads a file of at most [`LOG_MAX_SIZE`] (twice
/// that for Docker), so it is quick; a daemon that does not answer must not hold up the
/// result of a job that has already ended.
pub const LOGS_LIMIT: Duration = Duration::from_secs(30);

// The worker's bound for a call is CALL_LIMIT and more: the `logs` call is inside it.
const _: () = assert!(LOGS_LIMIT.as_secs() < super::CALL_LIMIT.as_secs());

/// The flags of `create` that choose the log driver and bound the log: Podman's
/// `k8s-file` driver (its default, `journald`, is unbounded here and needs a journal to
/// read back) with `max-size`, and Docker's `json-file` with `max-size` and a second file.
pub fn log_args(engine: Engine) -> Vec<String> {
    let size = format!("max-size={LOG_MAX_SIZE}");
    let args: &[&str] = match engine {
        Engine::Podman => &["--log-driver", "k8s-file", "--log-opt", &size],
        Engine::Docker => &[
            "--log-driver",
            "json-file",
            "--log-opt",
            &size,
            "--log-opt",
            "max-file=2",
        ],
    };
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The tail of the two streams as text: each at most [`MAX_LOG_STREAM_LEN`] bytes of
/// UTF-8, cut at a character boundary, with invalid bytes replaced. `dropped` says that
/// the reader already discarded older bytes, so the first bytes of a stream may be the
/// rest of a character and are skipped.
pub fn tail_of(streams: &Streams) -> LogTail {
    let (stdout, cut_out) = text_of(&streams.stdout, streams.dropped);
    let (stderr, cut_err) = text_of(&streams.stderr, streams.dropped);
    LogTail {
        stdout,
        stderr,
        truncated: streams.dropped || cut_out || cut_err,
    }
}

/// `bytes` as text of at most [`MAX_LOG_STREAM_LEN`] bytes, keeping the end, and whether
/// the end was cut again to fit.
fn text_of(bytes: &[u8], started_inside: bool) -> (String, bool) {
    let mut start = 0;
    if started_inside {
        // A character is at most 4 bytes: at most 3 continuation bytes are its rest.
        start = bytes
            .iter()
            .take(3)
            .take_while(|byte| (**byte & 0xC0) == 0x80)
            .count();
    }
    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    match text.len().checked_sub(MAX_LOG_STREAM_LEN) {
        // Replacing invalid bytes can lengthen the text (one byte becomes three).
        Some(over) if over > 0 => {
            let mut at = over;
            while !text.is_char_boundary(at) {
                at += 1;
            }
            (text[at..].to_owned(), true)
        }
        _ => (text, false),
    }
}

impl CliRuntime {
    /// The same runtime giving the `logs` call `limit` instead of [`LOGS_LIMIT`]: a test's
    /// seam.
    #[must_use]
    pub fn with_logs_limit(self, limit: Duration) -> Self {
        Self {
            logs_limit: limit,
            ..self
        }
    }

    /// The tail of the log of the container `id`, which has exited and is not removed yet.
    pub(super) fn read_logs(&self, id: &RunHandle) -> Result<Option<LogTail>, CliError> {
        check(id)?;
        let args = ["logs", "--", &id.0];
        let streams = self
            .runner
            .run_streams(
                self.settings.bin(),
                &args,
                &engine_env(&self.settings),
                MAX_LOG_STREAM_LEN,
                self.logs_limit,
            )
            .map_err(|why| CliError::Call { call: "logs", why })?;
        Ok(Some(tail_of(&streams)))
    }
}
