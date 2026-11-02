//! The command line of the hook:
//! `js-agent update-hook [--name=value ...] <step> [<artifact>] --params <file>`.
//!
//! The updater runs `<executable> <hook_args> <step> [args]` (host-updates design, 3.2). This
//! module parses what follows the verb: the flags the declaration's `hook_args` put before the
//! step ([`Flags`]), the step, the staged release directory (`prepare` only) and the file of
//! the `[hook.params]` table, which this hook always needs (the cordon's lease and cap are in
//! it). Flags only: no value comes from the environment.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use super::flags::Flags;

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

impl FromStr for Step {
    type Err = ArgsError;

    fn from_str(word: &str) -> Result<Step, ArgsError> {
        Step::ALL
            .into_iter()
            .find(|step| step.name() == word)
            .ok_or_else(|| ArgsError::UnknownStep(word.to_owned()))
    }
}

/// Why a command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// Nothing follows the verb.
    #[error("no step given")]
    MissingStep,
    /// The first word is not one of the five steps.
    #[error("unknown step {0:?}")]
    UnknownStep(String),
    /// `prepare` without the staged release directory.
    #[error("prepare needs the staged release directory")]
    MissingArtifact,
    /// An argument the step does not take (an artifact for any step but `prepare`, a second
    /// artifact, any other flag).
    #[error("unexpected argument {0:?}")]
    Unexpected(OsString),
    /// `--params` is the last word.
    #[error("--params needs a file")]
    MissingParamsFile,
    /// `--params` twice.
    #[error("--params given twice")]
    DuplicateParams,
    /// No `--params`: the hook cannot know the cordon's lease and cap without the table.
    #[error("--params <file> is required")]
    MissingParams,
    /// A word before the step that is not one of the [`Flags`].
    #[error("unknown flag {0:?} before the step")]
    UnknownFlag(String),
    /// A flag before the step without `=value`, or with an empty value.
    #[error("{0} needs a value, written {0}=<value>")]
    FlagValue(String),
    /// A flag before the step given twice.
    #[error("{0} given twice")]
    DuplicateFlag(String),
    /// A flag that names a path was given one that is not absolute.
    #[error("{0} must be an absolute path")]
    RelativeFlag(String),
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The flags before the step.
    pub flags: Flags,
    /// The step.
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The file of the `[hook.params]` table.
    pub params: PathBuf,
}

impl Invocation {
    /// Parses the arguments that follow the verb: the flags, then the step.
    ///
    /// # Errors
    /// [`ArgsError`] for a flag that is unknown, repeated or without a value, an unknown step,
    /// a missing or extra argument, or a missing `--params`.
    pub fn parse<I>(args: I) -> Result<Invocation, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args.into_iter();
        let mut flags = Flags::default();
        let first = loop {
            let word = args.next().ok_or(ArgsError::MissingStep)?;
            let text = word.to_string_lossy();
            if !text.starts_with("--") {
                break text.into_owned();
            }
            // A value is taken as the bytes given or not at all, never changed to fit text.
            if word.to_str().is_none() {
                return Err(ArgsError::UnknownFlag(text.into_owned()));
            }
            flags.take(&text)?;
        };
        let step: Step = first.parse()?;
        let (mut artifact, mut params) = (None, None);
        while let Some(arg) = args.next() {
            if arg == "--params" {
                let file = args.next().ok_or(ArgsError::MissingParamsFile)?;
                if params.replace(PathBuf::from(file)).is_some() {
                    return Err(ArgsError::DuplicateParams);
                }
            } else if step == Step::Prepare && artifact.is_none() {
                artifact = Some(PathBuf::from(arg));
            } else {
                return Err(ArgsError::Unexpected(arg));
            }
        }
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        let params = params.ok_or(ArgsError::MissingParams)?;
        Ok(Invocation {
            flags,
            step,
            artifact,
            params,
        })
    }
}
