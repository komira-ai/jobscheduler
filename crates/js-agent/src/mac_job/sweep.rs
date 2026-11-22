//! Ending every process a job started, with no user of its own to kill by.
//!
//! A job can leave its process group, its session and its process tree (a double fork, `setsid`,
//! a disclaimed spawn): measured on macOS 27, a process group and a walk of the parent links each
//! found 10 of a job's 13 processes. The child opens a new audit session before it drops to the
//! user, every process it starts inherits it, and `setaudit_addr` is documented as needing root,
//! so a process is taken as the job's when its audit session is the job's. Whether a job can
//! leave its audit session on macOS 27 is not yet measured; the user rule below is what this
//! prototype's cleanup relies on.
//!
//! This prototype runs one job at a time per user, and checks the user runs nothing before the
//! start, so every process of the user is the job's too. The sweep ends both, and records which
//! rule found each process, so a run shows whether the audit session alone would have been
//! enough.
//!
//! The decision is here, on every platform; the listing and the kill are the macOS module's.

use serde::Serialize;

/// The most sweep rounds: each lists every process again, so a fork between the listing and the
/// kill is found in the next one.
pub const ROUNDS_MAX: usize = 50;

/// A process, as the listing sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    /// Its id.
    pub pid: i32,
    /// Its effective user.
    pub uid: u32,
    /// Its audit session, if it could be read.
    pub asid: Option<i32>,
    /// Its name (the kernel's short command name).
    pub name: String,
}

/// A process the sweep ends, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its id.
    pub pid: i32,
    /// Its name.
    pub name: String,
    /// The rule that found it.
    pub why: Found,
}

/// A process the sweep ended, as the report names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Swept {
    /// Its id.
    pub pid: i32,
    /// Its name.
    pub name: String,
}

/// What the job's processes are known by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobIds {
    /// The user the job runs as.
    pub uid: u32,
    /// The job's own audit session, if it has one.
    pub asid: Option<i32>,
    /// The sweeping process, never a target.
    pub own_pid: i32,
}

/// Which rule found a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Found {
    /// It is in the job's audit session.
    Session,
    /// It is not, but it runs as the job's user.
    UserOnly,
}

/// The processes of `procs` to end, and why: in the job's audit session, or the job's user's.
/// Never the sweeping process, `launchd` or the kernel.
#[must_use]
pub fn targets(procs: &[Proc], job: &JobIds) -> Vec<Target> {
    procs
        .iter()
        .filter(|p| p.pid > 1 && p.pid != job.own_pid)
        .filter_map(|p| {
            let why = if job.asid.is_some() && p.asid == job.asid {
                Found::Session
            } else if p.uid == job.uid {
                Found::UserOnly
            } else {
                return None;
            };
            Some(Target {
                pid: p.pid,
                name: p.name.clone(),
                why,
            })
        })
        .collect()
}

/// What a sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// Processes found by the audit session.
    pub by_session: Vec<Swept>,
    /// Processes found by the user alone: on a Mac, the per-user daemons launchd starts for an
    /// account (their audit session is launchd's, not the job's).
    pub by_user_only: Vec<Swept>,
    /// The rounds that found something, and the one that found nothing.
    pub rounds: usize,
    /// Whether the last round found nothing.
    pub clean: bool,
    /// Why the sweep stopped before a round found nothing, when the process table could not be
    /// read.
    pub error: Option<String>,
}

impl Tally {
    /// Records one round's targets. Returns whether it found any.
    pub fn round(&mut self, found: &[Target]) -> bool {
        self.rounds += 1;
        for target in found {
            let list = match target.why {
                Found::Session => &mut self.by_session,
                Found::UserOnly => &mut self.by_user_only,
            };
            if !list.iter().any(|swept| swept.pid == target.pid) {
                list.push(Swept {
                    pid: target.pid,
                    name: target.name.clone(),
                });
            }
        }
        self.clean = found.is_empty();
        !found.is_empty()
    }

    /// Records that the process table could not be read: the sweep is not clean.
    pub fn failed(&mut self, why: String) {
        self.clean = false;
        self.error = Some(why);
    }
}

#[cfg(test)]
#[path = "sweep_tests.rs"]
mod tests;
