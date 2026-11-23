//! Ending a run's processes before its removal.
//!
//! This ends the job's process group only: `SIGKILL` to the group the supervisor started the
//! job in (its pid is the group's id). A process the job detached into a group or a session
//! of its own is ended by the account's retirement, which kills every process of the run's
//! uid until none is left ([`crate::mac_accounts::Pool::retire`]).

use rustix::process::{Pid, Signal, kill_process_group};

use super::run::Job;

/// Kills the process group of `job`'s program. A group that is gone already is no failure.
pub(super) fn end_processes(job: &Job) {
    signal_group(job, Signal::KILL);
}

/// Sends `signal` to the process group of `job`'s program, if its pid can name one.
pub(super) fn signal_group(job: &Job, signal: Signal) {
    let Some(group) = Pid::from_raw(job.ready.job_pid) else {
        return;
    };
    if let Err(why) = kill_process_group(group, signal) {
        tracing::debug!("native: {signal:?} to group {}: {why}", job.ready.job_pid);
    }
}
