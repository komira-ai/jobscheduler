//! The command line of the config hook:
//!
//! ```text
//! js-agent config-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! js-agent config-hook uninstall [--params <file>]
//! ```
//!
//! hostkeeper runs the hook as `<hook.command...> <step> ...`, and the package manifest's
//! `hook.command` ends in the verb, so the verb is never absent. This module parses what follows
//! it. The five update steps are the update hook's (js-update-hook's step names and the update
//! hook's [`Invocation`](crate::update_hook::Invocation)); `uninstall` is the updater's step for
//! a package that is removed, and the config hook does nothing in it, so it needs no params.

use std::ffi::OsString;

use crate::update_hook::{ArgsError, Invocation as StepInvocation};

/// The first argument that selects the config hook, with the step after it.
pub const VERB: &str = "config-hook";

/// What the hook was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `uninstall`: the service is left as it is.
    Uninstall,
    /// One of the five steps of an update, with its arguments.
    Step(StepInvocation),
}

impl Invocation {
    /// Parses the arguments after the verb.
    ///
    /// # Errors
    /// [`ArgsError`] for an unknown step, a missing or extra argument, or a missing `--params`
    /// (`uninstall` takes none).
    pub fn parse(words: Vec<OsString>) -> Result<Invocation, ArgsError> {
        if words.first().is_some_and(|word| word == "uninstall") {
            return match &words[1..] {
                [] => Ok(Invocation::Uninstall),
                [flag, _] if flag == "--params" => Ok(Invocation::Uninstall),
                [other, ..] => Err(ArgsError::Unexpected(other.clone())),
            };
        }
        StepInvocation::parse(words).map(Invocation::Step)
    }
}
