//! How long the agent lets the host settle before it counts what it sees against a job
//! (`--settle-ms`).
//!
//! A host does things of its own at the moment a job ends: its service manager forks the
//! processes of a timer's unit (their parent is `init` for the moment before they are moved
//! into the unit's control group), a container that has just exited is read as running for
//! a step more, and the worker steps again at once after every order, so two steps are no
//! time apart. What a job really left behind is still there a moment later, and a run the
//! agent really cannot see is still without a control group a moment later. So two judgements
//! wait this long:
//!
//! - a process the baseline did not have is a job's leftover only when it is still there
//!   after the wait (`daemonised-child`, `process-as-job-user`; see [`crate::integrity`]);
//! - a running run with no control group the runtime can name taints the host as unchecked
//!   only when it is still running without one this long after it was first seen so.

use std::time::Duration;

/// The wait when `--settle-ms` is not given: one second.
pub const DEFAULT_SETTLE: Duration = Duration::from_millis(1000);

/// The most milliseconds `--settle-ms` takes (a minute: the agent's worker waits this long
/// for each job whose check sees a new process).
pub const MAX_SETTLE_MS: u64 = 60_000;

/// The wait `--settle-ms` gave, or [`DEFAULT_SETTLE`].
///
/// # Errors
///
/// The text names the flag: the value is 0 or above [`MAX_SETTLE_MS`].
pub fn settle(given: Option<u64>) -> Result<Duration, String> {
    match given {
        None => Ok(DEFAULT_SETTLE),
        Some(0) => Err(
            "--settle-ms: at least 1: a judgement that does not wait counts what the host does at \
             the moment a job ends against the job"
                .to_owned(),
        ),
        Some(ms) if ms > MAX_SETTLE_MS => Err(format!("--settle-ms: at most {MAX_SETTLE_MS}")),
        Some(ms) => Ok(Duration::from_millis(ms)),
    }
}
