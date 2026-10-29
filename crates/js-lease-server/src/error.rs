//! The errors that end the process.

use std::fmt;
use std::io;
use std::process::ExitCode;

/// The server was refused permission to start: one flag is wrong, or one field of the
/// configuration file, and the error names it. The process exits with status 2.
///
/// A flag reads as `--listen: reason`. A field of the configuration file reads as its
/// place and the reason: `server.listen: reason`, or `pool "gpu": provider.api_key_file:
/// reason` for a field of a pool. Neither holds a value from the file.
#[derive(Debug)]
pub struct StartupError {
    flag: &'static str,
    place: Option<String>,
    reason: String,
}

impl fmt::Display for StartupError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = self.place.as_deref().unwrap_or(self.flag);
        write!(out, "{place}: {}", self.reason)
    }
}

impl std::error::Error for StartupError {}

impl StartupError {
    /// A refusal naming `flag` (with its leading dashes) for `reason`.
    pub fn new(flag: &'static str, reason: impl fmt::Display) -> Self {
        Self {
            flag,
            place: None,
            reason: reason.to_string(),
        }
    }

    /// A refusal of a field of the configuration file (`--config`): `place` is where it
    /// is, such as `server.listen` or `pool "gpu": provider.api_key_file`. [`flag`]
    /// answers `--config`.
    ///
    /// [`flag`]: StartupError::flag
    pub fn at(place: impl fmt::Display, reason: impl fmt::Display) -> Self {
        Self {
            flag: "--config",
            place: Some(place.to_string()),
            reason: reason.to_string(),
        }
    }

    /// A constructor for `map_err` that makes a refusal naming `flag` from any displayable
    /// error.
    pub fn because<E: fmt::Display>(flag: &'static str) -> impl FnOnce(E) -> Self {
        move |err| Self::new(flag, err)
    }

    /// The flag at fault, such as `--token-file`; `--config` for a field of that file.
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

    /// Asserts that a refusal of a field of the configuration file reads as its place and
    /// the reason, and answers `--config` as its flag.
    #[test]
    fn a_field_refusal_reads_as_its_place() {
        let refusal = StartupError::at("pool \"gpu\": max_instances", "required");
        assert_eq!(refusal.to_string(), "pool \"gpu\": max_instances: required");
        assert_eq!((refusal.flag(), refusal.reason()), ("--config", "required"));
    }

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
