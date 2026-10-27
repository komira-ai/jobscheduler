//! The errors that end the process.

use std::fmt;
use std::io;
use std::process::ExitCode;

/// The server was refused permission to start: one flag is wrong, and the error names
/// it. The process exits with status 2.
#[derive(Debug, thiserror::Error)]
#[error("{flag}: {reason}")]
pub struct StartupError {
    flag: &'static str,
    reason: String,
}

impl StartupError {
    /// A refusal naming `flag` (with its leading dashes) for `reason`.
    pub fn new(flag: &'static str, reason: impl fmt::Display) -> Self {
        Self {
            flag,
            reason: reason.to_string(),
        }
    }

    /// A constructor for `map_err` that makes a refusal naming `flag` from any displayable
    /// error.
    pub fn because<E: fmt::Display>(flag: &'static str) -> impl FnOnce(E) -> Self {
        move |err| Self::new(flag, err)
    }

    /// The flag at fault, such as `--token-file`.
    pub fn flag(&self) -> &'static str {
        self.flag
    }

    /// What is wrong with it.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Why the process is ending with an error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    /// A flag or a file it names is unusable. Exit status 2.
    #[error(transparent)]
    Startup(#[from] StartupError),
    /// The server could not run or stopped abnormally. Exit status 1.
    #[error("{0}")]
    Runtime(io::Error),
}

impl Failure {
    /// The process exit status for this failure.
    pub(crate) fn code(&self) -> ExitCode {
        match self {
            Self::Startup(_) => ExitCode::from(2),
            Self::Runtime(_) => ExitCode::FAILURE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a refusal exits 2 and any other failure 1, and that a refusal
    /// reads as its flag, a colon and the reason.
    #[test]
    fn failures_have_their_exit_status() {
        let refusal = StartupError::new("--listen", "in use");
        assert_eq!((refusal.flag(), refusal.reason()), ("--listen", "in use"));
        let startup = Failure::from(refusal);
        assert_eq!(startup.to_string(), "--listen: in use");
        assert!(startup.code() == ExitCode::from(2));
        let runtime = Failure::Runtime(io::Error::other("boom"));
        assert_eq!(runtime.to_string(), "boom");
        assert!(runtime.code() == ExitCode::FAILURE);
    }
}
