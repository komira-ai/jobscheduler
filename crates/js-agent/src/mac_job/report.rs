//! The report of one `__mac-job` run, written as JSON.

use serde::Serialize;

use super::status;
use super::sweep::Tally;

/// How the job's program ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "how", content = "value")]
pub enum Ended {
    /// It exited with this status.
    Exited(i32),
    /// A signal ended it.
    Signalled(i32),
    /// Its wall limit passed; the sweep ended it.
    WallLimit,
}

/// What is still there after the job folder is removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Leftovers {
    /// Processes of the user.
    pub processes: Vec<i32>,
    /// Files of the user outside the job folder that are new or changed since the start.
    pub files: Vec<String>,
    /// The job folder, if it could not be removed: why.
    pub folder: Option<String>,
}

impl Leftovers {
    /// Whether nothing is left.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.processes.is_empty() && self.files.is_empty() && self.folder.is_none()
    }
}

/// One run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The job's id.
    pub id: String,
    /// The user it ran as.
    pub user: String,
    /// The user's id.
    pub uid: u32,
    /// The job's own audit session, if it had one.
    pub asid: Option<i32>,
    /// The job folder.
    pub folder: String,
    /// The profile text the job ran under.
    pub profile: String,
    /// How the program ended.
    pub ended: Ended,
    /// Milliseconds from the start to the program's end.
    pub run_ms: u64,
    /// What the sweep found, and by which rule.
    pub sweep: Tally,
    /// The outputs copied, and the ones that were missing.
    pub outputs: Outputs,
    /// What was left.
    pub leftovers: Leftovers,
    /// The lent account and what its retirement did, when the pool lent one.
    pub account: Option<AccountUse>,
    /// How the program was started: `ordinary` or `disclaimed`.
    pub spawn: String,
    /// The last [`TAIL_MAX`] bytes of the program's standard output.
    pub stdout_tail: String,
    /// The last [`TAIL_MAX`] bytes of its standard error.
    pub stderr_tail: String,
}

/// An account the pool lent for the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountUse {
    /// Its name.
    pub name: String,
    /// Whether none was ready and it was made for this job.
    pub made_now: bool,
    /// What its retirement did, or why the account is held.
    pub retired: Result<String, String>,
}

/// The most of each stream a report carries.
pub const TAIL_MAX: usize = 4096;

/// The last [`TAIL_MAX`] bytes of `bytes`, as text (a cut character reads as U+FFFD).
#[must_use]
pub fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(TAIL_MAX);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// The declared outputs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Outputs {
    /// Copied to the output folder.
    pub copied: Vec<String>,
    /// Missing, or not copied, with why.
    pub missing: Vec<String>,
}

impl Report {
    /// The mode's status for this run.
    #[must_use]
    pub fn status(&self) -> u8 {
        let held = self.account.as_ref().is_some_and(|a| a.retired.is_err());
        if held || !self.leftovers.is_clean() || !self.sweep.clean {
            status::LEFT_BEHIND
        } else if self.ended == Ended::Exited(0) && self.outputs.missing.is_empty() {
            status::OK
        } else {
            status::JOB_FAILED
        }
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
