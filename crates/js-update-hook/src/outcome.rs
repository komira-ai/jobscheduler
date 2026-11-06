//! How a step ended, as the updater reads it: the exit status.

use std::process::ExitCode;

// These are the updater's exit codes of a step. A service may give the same numbers other
// meanings elsewhere (js-agent's readiness hook exits 75 and 76 too); the sets share numbers
// and nothing else.

/// How a step ended, as the updater reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Success,
    /// Exit 1.
    Failure,
    /// Exit 75: the host is busy or not ready, nothing irreversible happened, try later.
    Deferred,
    /// Exit 76: no version can repair this; acting further would make it worse.
    Halt,
}

impl Outcome {
    /// The status a step exits with.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Outcome::Success => 0,
            Outcome::Failure => 1,
            Outcome::Deferred => 75,
            Outcome::Halt => 76,
        }
    }
}

impl From<Outcome> for ExitCode {
    fn from(outcome: Outcome) -> ExitCode {
        ExitCode::from(outcome.code())
    }
}
