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
    /// The user it ran as, and that user's id.
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
        if !self.leftovers.is_clean() || !self.sweep.clean {
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
