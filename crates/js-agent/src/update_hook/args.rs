//! The command line of the hook, in the two forms the updater uses:
//!
//! ```text
//! js-agent update-hook <step> [<artifact>] --params <file>
//! js-agent <step> [<artifact>] --params <file>
//! ```
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

pub use js_update_hook::Step;
use js_update_hook::UnknownStep;

/// The first argument that selects the hook, with the step after it.
pub const VERB: &str = "update-hook";

/// The step words the updater passes without the verb. `uninstall` is the updater's step for
/// a service that has a hook to remove it with: this hook has none, and refuses it.
const BARE_WORDS: [&str; 6] = [
    "prepare",
    "cutover",
    "verify",
    "rollback",
    "cleanup",
    "uninstall",
];

/// Whether `word`, the first argument after the program name, selects the hook: the verb, or
/// a step word given bare.
#[must_use]
pub fn is_hook_word(word: &OsStr) -> bool {
    word == VERB || BARE_WORDS.iter().any(|step| word == *step)
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
            step,
            artifact,
            params,
        })
    }
}
