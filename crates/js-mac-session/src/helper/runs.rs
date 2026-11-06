//! The runs this helper started for lease users, so that a run outlives the connection of
//! the agent that asked for it: `watch` finds a run again for an agent that restarted,
//! `stop` ends one softly, and a run started with a deadline is ended by the helper when the
//! deadline passes, whether or not an agent is still connected.
//!
//! The table holds, for each lease, the last process `run` started for it: its pid (which
//! leads a process group of its own, so it is the group's id), how it ended once it has,
//! and its deadline, if it has one. It lives in the helper's memory: a helper that restarts
//! starts with an empty table, and its boot reset deletes every lease user, so a run of an
//! earlier helper is never found again. An entry is dropped when its lease's user is deleted.
//!
//! - `watch <lease>` answers as `run` does: `started` with the pid, then `exited` when the
//!   process ends (at once when it has ended already). A lease with no run in the table is
//!   refused ([`NO_RUN`]).
//! - `stop <lease> <grace>` sends `SIGTERM` to the run's process group and waits, by the
//!   host's clock, until the process ends or the grace has passed; only then, if it still
//!   runs, it does what `kill-uid` does. A run that ended on the `SIGTERM` is never killed.
//!   A lease with no running process in the table is killed at once.
//! - A deadline (`run`'s `deadline_ms`, counted from the start on the host's clock) is held
//!   by the thread that waits for the process: when it passes and the process still runs,
//!   the helper does what `kill-uid` does.

use std::collections::BTreeMap;
use std::process::{Child, ExitStatus};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use js_types::LeaseId;

use super::Helper;
use crate::lease::{Dotted, parse_lease};

/// What `watch` is refused with for a lease that has no run in this helper's table.
pub const NO_RUN: &str = "has no run this helper started";

/// The longest grace or deadline the helper counts; a longer one is cut to it (a century,
/// so a time past it is never in reach and the clock's arithmetic never overflows).
pub const MAX_WAIT: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// How a process ended: its exit code, or the signal that ended it.
pub type Ended = (Option<i32>, Option<i32>);

/// One lease's last run.
#[derive(Clone, Copy, Debug)]
struct Entry {
    pid: u32,
    ended: Option<Ended>,
    deadline: Option<SystemTime>,
}

/// The table and the condition every change to it is told on.
#[derive(Default)]
pub(super) struct Runs {
    table: Mutex<BTreeMap<LeaseId, Entry>>,
    changed: Condvar,
}

impl Runs {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<LeaseId, Entry>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits on the table for at most `left`, or until it changes.
    fn wait<'a>(
        &self,
        table: MutexGuard<'a, BTreeMap<LeaseId, Entry>>,
        left: Duration,
    ) -> MutexGuard<'a, BTreeMap<LeaseId, Entry>> {
        let waited = self.changed.wait_timeout(table, left);
        waited.unwrap_or_else(PoisonError::into_inner).0
    }

    /// Records that `pid` started for `lease`, ending at `deadline` if it has one.
    pub(super) fn started(&self, lease: LeaseId, pid: u32, deadline: Option<SystemTime>) {
        let entry = Entry {
            pid,
            ended: None,
            deadline,
        };
        self.lock().insert(lease, entry);
        self.changed.notify_all();
    }

    /// Records how `pid` ended, if the table holds it.
    pub(super) fn ended(&self, pid: u32, how: Ended) {
        let mut table = self.lock();
        if let Some(entry) = table.values_mut().find(|entry| entry.pid == pid) {
            entry.ended = Some(how);
        }
        drop(table);
        self.changed.notify_all();
    }

    /// Drops the entry of `lease`.
    pub(super) fn forget(&self, lease: LeaseId) {
        self.lock().remove(&lease);
    }
}

/// How `status` ended, as a reply tells it.
fn ended_of(status: ExitStatus) -> Ended {
    use std::os::unix::process::ExitStatusExt as _;
    (status.code(), status.signal())
}

impl Helper {
    /// Waits for `child`, a process `run` or `role-run` started, and records how it ended. A
    /// run of a lease with a deadline is ended by the helper when the deadline passes
    /// ([`Helper::hold_deadline`]) while this waits.
    ///
    /// # Errors
    /// The process could not be waited for.
    pub fn wait(&self, child: &mut Child) -> std::io::Result<ExitStatus> {
        let pid = child.id();
        std::thread::scope(|scope| {
            scope.spawn(|| self.hold_deadline(pid));
            let status = child.wait();
            // The holder looks again on any change: an end it cannot read is an end too.
            let how = status
                .as_ref()
                .map_or((None, None), |status| ended_of(*status));
            self.runs.ended(pid, how);
            status
        })
    }

    /// Holds the deadline of the run whose process is `pid`, if the table has one: returns
    /// once the run has ended, or ends it (as `kill-uid` does) once the host's clock is past
    /// its deadline. A run with no deadline, or not in the table, returns at once.
    pub(super) fn hold_deadline(&self, pid: u32) {
        let mut table = self.runs.lock();
        loop {
            let Some((lease, entry)) = table.iter().find(|(_, entry)| entry.pid == pid) else {
                return;
            };
            let (lease, entry) = (*lease, *entry);
            let (None, Some(deadline)) = (entry.ended, entry.deadline) else {
                return;
            };
            let left = deadline
                .duration_since(self.host.now())
                .unwrap_or(Duration::ZERO);
            if left.is_zero() {
                drop(table);
                let lease = Dotted(lease).to_string();
                tracing::info!(lease, pid, "the run's deadline passed; ending it");
                if let Err(why) = self.kill_uid(&lease) {
                    tracing::warn!(lease, pid, "ending the run at its deadline: {why}");
                }
                return;
            }
            table = self.runs.wait(table, left);
        }
    }

    /// Wakes every wait on the runs' table to look at the host's clock again. The waits time
    /// out by themselves on the real clock; a test moves its own clock and calls this.
    #[cfg(test)]
    pub(crate) fn clock_moved(&self) {
        // Under the lock: a wait that read the clock before it moved is waiting by now.
        let table = self.runs.lock();
        self.runs.changed.notify_all();
        drop(table);
    }

    /// `run`'s deadline: records that the last run of `lease` ends `after` its start, by the
    /// host's clock.
    pub(super) fn set_deadline(&self, lease: &str, pid: u32, after: Duration) {
        let deadline = self.host.now() + after.min(MAX_WAIT);
        // `run` parsed the lease before it started the process.
        let _ = parse_lease(lease).map(|lease| self.runs.started(lease, pid, Some(deadline)));
    }

    /// Drops the run of `lease` from the table, once its user is deleted.
    pub(super) fn forget_run(&self, lease: &str) {
        // `user-delete` parsed the lease before it deleted the user.
        let _ = parse_lease(lease).map(|lease| self.runs.forget(lease));
    }

    /// `watch <lease>`: the pid of the last run of `lease`.
    ///
    /// # Errors
    /// The lease is malformed, or has no run in the table.
    pub fn watch(&self, lease: &str) -> Result<u32, String> {
        let lease = parse_lease(lease)?;
        match self.runs.lock().get(&lease) {
            Some(entry) => Ok(entry.pid),
            None => Err(format!("lease {} {NO_RUN}", Dotted(lease))),
        }
    }

    /// Waits until the last run of `lease` has ended, and says how; `None` when the table
    /// has no run of the lease (any more).
    pub fn wait_watched(&self, lease: &str) -> Option<Ended> {
        let lease = parse_lease(lease).ok()?;
        let mut table = self.runs.lock();
        loop {
            match table.get(&lease)?.ended {
                Some(how) => return Some(how),
                None => {
                    table = self
                        .runs
                        .changed
                        .wait(table)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    /// `stop <lease> <grace>`: see the module documentation.
    ///
    /// # Errors
    /// The lease has no live user, the signal could not be sent, or the kill failed.
    pub fn stop(&self, lease: &str, grace: Duration) -> Result<(), String> {
        let id = parse_lease(lease)?;
        {
            let ledger = self.lock();
            self.live_uid(&ledger, id)?;
        }
        let running = self
            .runs
            .lock()
            .get(&id)
            .filter(|entry| entry.ended.is_none())
            .map(|entry| entry.pid);
        let Some(pid) = running.map(u32::cast_signed) else {
            return self.kill_uid(lease);
        };
        let until = self.host.now() + grace.min(MAX_WAIT);
        self.host
            .term_group(pid)
            .map_err(|why| format!("signalling process group {pid}: {why}"))?;
        let mut table = self.runs.lock();
        loop {
            if table.get(&id).is_none_or(|entry| entry.ended.is_some()) {
                tracing::info!(lease, pid, "the run ended on SIGTERM");
                return Ok(());
            }
            let left = until
                .duration_since(self.host.now())
                .unwrap_or(Duration::ZERO);
            if left.is_zero() {
                break;
            }
            table = self.runs.wait(table, left);
        }
        drop(table);
        tracing::info!(lease, pid, "the run outlived its grace; killing its user");
        self.kill_uid(lease)
    }
}

#[cfg(test)]
mod tests;
