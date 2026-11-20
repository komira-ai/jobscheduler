//! The steps of the hook contract, by the words the updater puts on the command line: the five
//! steps of an update ([`Step`]) and the two of a maintenance run ([`MaintenanceStep`]).

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

/// A word that is not one of the hook's steps.
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

/// One of the two steps of a maintenance run: they take the service out of service and put it
/// back, around work on the host that is not an update of the service (hostkeeper's
/// maintenance-updates design, 5.2). They are not steps of an update: a package's manifest
/// names them (`[hook] drain` and `[hook] resume`), and the updater runs each alone, with a
/// params file of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceStep {
    /// Take the service out of service, and return once it has no work.
    Drain,
    /// Put the service back.
    Resume,
}

impl MaintenanceStep {
    /// Both steps, in the order a maintenance run runs them.
    pub const ALL: [MaintenanceStep; 2] = [MaintenanceStep::Drain, MaintenanceStep::Resume];

    /// The word the updater puts on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            MaintenanceStep::Drain => "drain",
            MaintenanceStep::Resume => "resume",
        }
    }
}

impl fmt::Display for MaintenanceStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for MaintenanceStep {
    type Err = UnknownStep;

    fn from_str(word: &str) -> Result<MaintenanceStep, UnknownStep> {
        MaintenanceStep::ALL
            .into_iter()
            .find(|step| step.name() == word)
            .ok_or_else(|| UnknownStep(word.to_owned()))
    }
}
