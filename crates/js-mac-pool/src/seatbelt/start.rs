//! The command line of js-agent's start mode, read back into what it applies and runs.

use std::ffi::{OsStr, OsString};

use super::apply::{InitError, SandboxInit};

/// Why the arguments of the start mode are not a profile and a program.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// No `--profile` before the `--`.
    #[error("no --profile was given")]
    NoProfile,
    /// `--profile` was given more than once.
    #[error("--profile was given twice")]
    TwoProfiles,
    /// An option is the last argument and has no value; the field is the option.
    #[error("{0} has no value")]
    MissingValue(String),
    /// A `--param` value has no name before its first `=` (or no `=` at all); the field
    /// is the value as given.
    #[error("--param {0} is not NAME=VALUE with a name")]
    ParamWithoutName(String),
    /// The value of an option is not UTF-8 text; the field is the option.
    #[error("the value of {0} is not UTF-8 text")]
    NotText(String),
    /// An argument before the `--` is neither `--profile` nor `--param`.
    #[error("unknown option {0}")]
    UnknownOption(String),
    /// The arguments end before a `--`.
    #[error("no -- before the program")]
    NoSeparator,
    /// Nothing follows the `--`.
    #[error("no program after --")]
    NoProgram,
    /// The profile or a parameter holds a NUL byte.
    #[error(transparent)]
    Init(#[from] InitError),
}

/// What js-agent's start mode applies and executes: a profile and its parameters as the
/// C strings of the in-process apply, the program, and its arguments.
///
/// The grammar, after the start verb: `--profile TEXT` exactly once and any number of
/// `--param NAME=VALUE` (split at the first `=`; the name is not empty; the value may
/// hold `=`), in any order; then `--`; then the program and its arguments, which are
/// taken as they are. The profile text and every parameter are UTF-8. This is the
/// inverse of [`Profile::start_argv`](super::Profile::start_argv).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartArgs {
    init: SandboxInit,
    program: OsString,
    args: Vec<OsString>,
}

impl StartArgs {
    /// Reads `args`, everything after the start verb.
    ///
    /// # Errors
    /// A [`StartError`] naming what is wrong: a missing, repeated or unknown option, a
    /// value that is missing or not text, a missing `--` or program, or a NUL byte in
    /// the profile or a parameter.
    pub fn parse(args: &[OsString]) -> Result<Self, StartError> {
        let mut rest = args.iter();
        let mut profile: Option<String> = None;
        let mut params: Vec<(String, String)> = Vec::new();
        loop {
            let Some(arg) = rest.next() else {
                return Err(StartError::NoSeparator);
            };
            match arg.to_string_lossy().as_ref() {
                "--" => break,
                "--profile" => {
                    let text = text_of("--profile", rest.next())?;
                    if profile.replace(text).is_some() {
                        return Err(StartError::TwoProfiles);
                    }
                }
                "--param" => {
                    let text = text_of("--param", rest.next())?;
                    match text.split_once('=') {
                        Some((name, value)) if !name.is_empty() => {
                            params.push((name.to_owned(), value.to_owned()));
                        }
                        _ => return Err(StartError::ParamWithoutName(text)),
                    }
                }
                other => return Err(StartError::UnknownOption(other.to_owned())),
            }
        }
        let profile = profile.ok_or(StartError::NoProfile)?;
        let program = rest.next().ok_or(StartError::NoProgram)?.clone();
        Ok(Self {
            init: SandboxInit::new(&profile, &params)?,
            program,
            args: rest.cloned().collect(),
        })
    }

    /// The profile and its parameters, as the in-process apply takes them.
    #[must_use]
    pub fn init(&self) -> &SandboxInit {
        &self.init
    }

    /// The program to execute once the profile is applied, as given:
    /// [`Profile::start_argv`](super::Profile::start_argv) writes an absolute one, and this
    /// does not check it, so a relative one is searched for in the job's `PATH`, inside the
    /// applied profile.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// The program's arguments, not including the program.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// The value of `option` as text.
fn text_of(option: &str, value: Option<&OsString>) -> Result<String, StartError> {
    let value = value.ok_or_else(|| StartError::MissingValue(option.to_owned()))?;
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| StartError::NotText(option.to_owned()))
}
