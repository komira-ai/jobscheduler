//! The command line of the hook, in the two forms the updater uses:
//!
//! ```text
//! js-agent update-hook <step> [<artifact>] --params <file>
//! js-agent <step> [<artifact>] --params <file>
//! js-agent update-hook <drain|resume> --params <file>
//! js-agent <drain|resume> --params <file>
//! ```
//!
//! A step of an update is an [`Invocation`]; `drain` and `resume`, the steps of a maintenance
//! run, are a [`MaintainInvocation`], and take no artifact. [`Request`] reads either.
//!
//! hostkeeper runs the update steps (`prepare`, `cutover`, `verify`, `rollback`, `cleanup`) as
//! `<executable> <step> ...` and gives the leading arguments of the manifest's `command` only
//! to the steps it adds (`boot`, `uninstall`), so the hook takes the step with or without the
//! verb before it. This module parses what follows the verb: the step, the staged release
//! directory (`prepare` only) and the file of the `[params]` table, which this hook always
//! needs (the cordon's lease and cap are in it). Flags only: no value comes from the
//! environment, and no flag precedes the step.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use js_update_hook::UnknownStep;
pub use js_update_hook::{MaintenanceStep, Step};

/// The first argument that selects the hook, with the step after it.
pub const VERB: &str = "update-hook";

/// The step words the updater passes without the verb. `uninstall` is the updater's step for
/// a service that has a hook to remove it with: this hook has none, and refuses it.
const BARE_WORDS: [&str; 8] = [
    "prepare",
    "cutover",
    "verify",
    "rollback",
    "cleanup",
    "uninstall",
    "drain",
    "resume",
];

/// Whether `word`, the first argument after the program name, selects the hook: the verb, a
/// step word given bare, or the verb of the config package's hook (`config-hook`).
#[must_use]
pub fn is_hook_word(word: &OsStr) -> bool {
    word == VERB || word == super::config::VERB || BARE_WORDS.iter().any(|step| word == *step)
}

/// Why a command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// Nothing follows the verb.
    #[error("no step given")]
    MissingStep,
    /// The first word is not a step of an update or of a maintenance run.
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
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The step.
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The file of the `[params]` table.
    pub params: PathBuf,
}

/// A maintenance step the updater asked for, with its params file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainInvocation {
    /// `drain` or `resume`.
    pub step: MaintenanceStep,
    /// The file of the `[params]` table.
    pub params: PathBuf,
}

/// What a command line asks the hook for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A step of an update.
    Update(Invocation),
    /// A step of a maintenance run.
    Maintain(MaintainInvocation),
}

impl Request {
    /// Parses the arguments after the program name: the verb if it is there, then the step,
    /// which is a maintenance step's word or else read as [`Invocation::parse`] reads it.
    ///
    /// # Errors
    /// [`ArgsError`]: what [`Invocation::parse`] refuses, or for `drain` and `resume` an
    /// argument besides `--params <file>`, or none.
    pub fn parse<I>(args: I) -> Result<Request, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut words: Vec<OsString> = args.into_iter().collect();
        let at = usize::from(words.first().is_some_and(|word| word == VERB));
        let maintenance = words
            .get(at)
            .and_then(|word| word.to_str())
            .and_then(|word| word.parse::<MaintenanceStep>().ok());
        match maintenance {
            Some(step) => {
                let params = params_flag(words.split_off(at + 1), |arg| {
                    Err(ArgsError::Unexpected(arg))
                })?;
                let params = params.ok_or(ArgsError::MissingParams)?;
                Ok(Request::Maintain(MaintainInvocation { step, params }))
            }
            None => Invocation::parse_words(words).map(Request::Update),
        }
    }
}

/// Reads `--params <file>` out of `args`: each other argument is given to `other`, which keeps
/// it or refuses it.
fn params_flag(
    args: Vec<OsString>,
    mut other: impl FnMut(OsString) -> Result<(), ArgsError>,
) -> Result<Option<PathBuf>, ArgsError> {
    let mut args = args.into_iter();
    let mut params = None;
    while let Some(arg) = args.next() {
        if arg == "--params" {
            let file = args.next().ok_or(ArgsError::MissingParamsFile)?;
            if params.replace(PathBuf::from(file)).is_some() {
                return Err(ArgsError::DuplicateParams);
            }
        } else {
            other(arg)?;
        }
    }
    Ok(params)
}

impl Invocation {
    /// Parses the arguments after the program name: the verb if it is there, then the step.
    ///
    /// # Errors
    /// [`ArgsError`] for an unknown step (`uninstall` among them), a missing or extra
    /// argument, or a missing `--params`.
    pub fn parse<I>(args: I) -> Result<Invocation, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        Invocation::parse_words(args.into_iter().collect())
    }

    /// [`Invocation::parse`] on the words, one body for every caller's iterator.
    fn parse_words(words: Vec<OsString>) -> Result<Invocation, ArgsError> {
        let mut args = words.into_iter();
        let mut first = args.next().ok_or(ArgsError::MissingStep)?;
        if first == VERB {
            first = args.next().ok_or(ArgsError::MissingStep)?;
        }
        let step: Step = first
            .to_string_lossy()
            .parse()
            .map_err(|UnknownStep(word)| ArgsError::UnknownStep(word))?;
        let mut artifact = None;
        let params = params_flag(args.collect(), |arg| {
            if step == Step::Prepare && artifact.is_none() {
                artifact = Some(PathBuf::from(arg));
                Ok(())
            } else {
                Err(ArgsError::Unexpected(arg))
            }
        })?;
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        let params = params.ok_or(ArgsError::MissingParams)?;
        Ok(Invocation {
            step,
            artifact,
            params,
        })
    }
}
