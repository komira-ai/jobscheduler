//! The five steps of the hook contract, by the words the updater puts on the command line.

use std::fmt;
use std::str::FromStr;

/// One of the five steps, in the order a successful update runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Check the host can take the update and make it ready; nothing a client sees changes.
    Prepare,
    /// The visible change.
    Cutover,
    /// Observe that the new version serves.
    Verify,
    /// Undo what `prepare` and `cutover` did, as far as they can be undone.
    Rollback,
    /// Remove what only the update needed.
    Cleanup,
}

impl Step {
    /// Every step, in the order of the contract's table.
    pub const ALL: [Step; 5] = [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
    ];

    /// The word the updater puts on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Step::Prepare => "prepare",
            Step::Cutover => "cutover",
            Step::Verify => "verify",
            Step::Rollback => "rollback",
            Step::Cleanup => "cleanup",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A word that is not one of the five steps.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown step {0:?}")]
pub struct UnknownStep(pub String);

impl FromStr for Step {
    type Err = UnknownStep;

    fn from_str(word: &str) -> Result<Step, UnknownStep> {
        Step::ALL
            .into_iter()
            .find(|step| step.name() == word)
            .ok_or_else(|| UnknownStep(word.to_owned()))
    }
}
